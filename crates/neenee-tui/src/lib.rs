pub mod clipboard;
pub mod document;
pub mod fuzzy;
pub mod input;
pub mod layout;
pub mod render;
pub mod selection;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use neenee_core::{
    mcp::McpConnectionStatus, AgentMode, AgentRequest, AgentResponse, Goal, HarnessSnapshot,
    ImagePart, Message, PermissionDecision, PermissionRequest, Role, SessionOverview,
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    Terminal,
};
use std::{
    collections::HashMap,
    error::Error,
    io,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    sync::Arc,
};
use tokio::sync::{mpsc, Mutex};
use unicode_width::UnicodeWidthStr;

use crate::document::{MessageKind, TranscriptMessage};
use crate::layout::{
    InteractiveTarget, InteractiveTargetKind, LayoutMap, THINKING_BLOCK_IDX, TOOL_STEP_BLOCK_IDX,
};
use crate::render::Theme;
use crate::selection::{
    floor_char_boundary, get_selected_text, inclusive_end, SelectionDrag, SelectionState,
};

// Canonical command list. The descriptions here are the single source of
// truth and must stay in sync with the `/help` text in `crates/neenee/src/main.rs`
// and `docs/reference/commands.md`. Order is logical grouping, not alphabetical.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/models", "Select an LLM provider"),
    ("/mode", "Show or switch mode (build, plan)"),
    ("/mcp", "Show configured MCP server status"),
    ("/compact", "Compact older complete turns now"),
    ("/clear", "Clear the conversation history"),
    ("/permissions", "Show or clear always-allowed tool rules"),
    ("/session", "Manage durable sessions"),
    ("/sessions", "Browse past sessions"),
    ("/resume", "Resume the most recent or selected session"),
    ("/goal", "Set, inspect, complete, or clear the active goal"),
    ("/loop", "Run or resume bounded autonomous goal work"),
    ("/init", "Initialize a .neenee/ config tree"),
    ("/help", "Show available commands and keybindings"),
    ("/exit", "Exit the program"),
];

/// Kind of completion menu the input box is currently offering. Drives the
/// keyboard shortcuts that cycle / accept entries: Tab, ↑/↓, and (for slash
/// only) plain Enter on a unique prefix. Path mentions only complete via Tab
/// so a plain Enter still sends the message as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionKind {
    /// No completion menu is active.
    #[default]
    None,
    /// `/command` and subcommand completion (replaces the whole input).
    Slash,
    /// `@path` file mention completion (splices into the input at the cursor).
    Path,
}

/// A single completion candidate rendered in the completion menu. The
/// `replace_start..replace_end` byte range is the slice of the current input
/// that gets overwritten by `label` when the candidate is accepted, so slash
/// commands (which replace the whole input) and inline `@path` mentions
/// (which replace only the `@prefix` token) share one accept path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Text to insert at the replace range.
    pub label: String,
    /// Hint shown to the right of the label (e.g. "Set goal", "dir", "1.2k").
    pub description: String,
    /// Byte offset in `App::input` where the replacement starts.
    pub replace_start: usize,
    /// Byte offset in `App::input` where the replacement ends.
    pub replace_end: usize,
}

impl Completion {
    /// Build a slash-command style completion that replaces the whole input
    /// (`replace_start = 0`, `replace_end = input_len`).
    fn whole_input(label: &str, description: &str, input_len: usize) -> Completion {
        Completion {
            label: label.to_string(),
            description: description.to_string(),
            replace_start: 0,
            replace_end: input_len,
        }
    }
}

/// Upper bound on the number of filesystem entries scanned for a single `@`
/// mention completion. Bounds the work on huge directories (e.g. generated
/// `node_modules`) so each keystroke stays imperceptible; the menu renders the
/// first six and cycles through the rest with ↑/↓.
const MAX_PATH_COMPLETIONS: usize = 200;

/// Cached recursive project listing for `@path` completion. Entries are
/// normalized to forward-slash paths relative to the captured cwd:
/// directories get a trailing `/`, files do not. Built once by
/// [`scan_project_files`] (ripgrep-first, manual walk fallback) and reused
/// across keystrokes, mirroring the per-directory picker cache in opencode's
/// TUI so each keystroke only filters instead of re-scanning.
#[derive(Debug, Clone)]
pub struct PathScan {
    pub entries: Vec<String>,
}

/// Recursively list files (and synthesized directory entries) under `cwd`,
/// respecting `.gitignore` and `.ignore`. Hidden files are included by
/// default so the user can mention e.g. `.env`; `.git/` is always excluded.
///
/// Prefers `rg --files` (fast, gitignore-aware, already a project dep) and
/// falls back to a manual recursive walk when `rg` is unavailable so the
/// feature still works on stripped systems. Matches the ripgrep-fallback
/// behaviour opencode uses when its native `fff` picker is missing.
fn scan_project_files(cwd: &std::path::Path) -> PathScan {
    let entries = try_ripgrep_scan(cwd).unwrap_or_else(|| manual_walk(cwd));
    PathScan { entries }
}

/// Ripgrep-backed project scan. Returns `None` if `rg` cannot be spawned or
/// exits non-zero so the caller can fall back to [`manual_walk`].
fn try_ripgrep_scan(cwd: &std::path::Path) -> Option<Vec<String>> {
    let output = std::process::Command::new("rg")
        .args([
            "--files",
            "--hidden",
            "--glob=!.git",
            "--color=never",
            "--no-messages",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.replace('\\', "/"))
        .collect();

    // Synthesize directory entries by walking each file's ancestor chain —
    // `rg --files` only emits files, so directories are derived. Matches
    // opencode's ripgrep-fallback behaviour.
    let mut dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for path in &files {
        let mut acc = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        // All but the last segment (the filename) are directory ancestors.
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            dirs.insert(format!("{}/", acc));
        }
    }

    let mut entries: Vec<String> = files;
    entries.extend(dirs);
    // Dirs first (alphabetic), then files (alphabetic). Case-insensitive to
    // keep `README.md` and `readme.md` adjacent on case-insensitive FSes.
    entries.sort_by(|a, b| {
        let a_dir = a.ends_with('/');
        let b_dir = b.ends_with('/');
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    entries.dedup();
    Some(entries)
}

/// Pure-Rust recursive directory walk used when `rg` is unavailable. Skips
/// `.git/` unconditionally; hidden files and other ignored directories are
/// included so users can still mention e.g. `.env` or `.github/workflows`.
fn manual_walk(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel_prefix)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = match entry.file_name().to_str() {
                Some(s) => s.to_string(),
                None => continue,
            };
            // `.git/` is always skipped to avoid dumping the entire repo
            // internals into the completion list.
            if name == ".git" {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let rel = if rel_prefix.is_empty() {
                name.clone()
            } else {
                format!("{}{}", rel_prefix, name)
            };
            if is_dir {
                let child_rel = format!("{}/", rel);
                stack.push((entry.path(), child_rel.clone()));
                out.push(child_rel);
            } else {
                out.push(rel);
            }
        }
    }
    out.sort_by(|a, b| {
        let a_dir = a.ends_with('/');
        let b_dir = b.ends_with('/');
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    out
}

/// Split a raw `@`-mention body (everything after the `@`) into the directory
/// to scan and the file-name prefix to match inside it. Kept for tests that
/// exercise the legacy single-directory resolution; the live completion path
/// uses the cached recursive scan + [`path_query_match`] instead.
#[cfg(test)]
fn split_prefix(after_at: &str, cwd: &std::path::Path) -> (std::path::PathBuf, String) {
    let last_slash = after_at.bytes().rposition(|b| b == b'/');
    let (dir_part, file_prefix) = match last_slash {
        Some(idx) => (&after_at[..=idx], after_at[idx + 1..].to_string()),
        None => ("", after_at.to_string()),
    };

    let base_dir = if let Some(rest) = dir_part.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => cwd.join(dir_part),
        }
    } else if dir_part.starts_with('/') {
        std::path::PathBuf::from(dir_part)
    } else {
        cwd.join(dir_part)
    };
    (base_dir, file_prefix)
}

/// Format a byte count with a single-letter SI suffix, matching the
/// context-usage formatter's style: `512B`, `1.2k`, `3.4M`.
#[cfg(test)]
fn format_byte_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}k", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Decide whether a cached path entry should be shown for a given `@query`.
///
/// - Empty query: only top-level entries (immediate children of cwd), so the
///   initial menu is a small, useful overview instead of every nested file.
/// - Query without `/`: case-insensitive substring match anywhere in the
///   path, so `@foo` finds `src/foo.rs` and `Cargo.lock` alike.
/// - Query ending in `/` (e.g. `@src/`): case-insensitive prefix match,
///   listing that directory's descendants so the user can descend naturally.
/// - Other queries: case-insensitive substring match — covers `@src/foo` and
///   similar mid-path fragments.
fn path_query_match(path: &str, query: &str) -> bool {
    if query.is_empty() {
        // Top-level: a path with no `/`, or a single trailing `/` and nothing
        // else (top-level directory).
        let trimmed = path.trim_end_matches('/');
        !trimmed.contains('/')
    } else if let Some(dir_prefix) = query.strip_suffix('/').filter(|_| query.contains('/')) {
        // Query is `@<dir>/`: descend, prefix match.
        path.to_lowercase().starts_with(&dir_prefix.to_lowercase())
    } else {
        path.to_lowercase().contains(&query.to_lowercase())
    }
}

/// Pure core of [`App::active_mention_range`]. Given the input bytes and a
/// byte offset sitting at the caret, return the inclusive `(start, end)` range
/// of the `@mention` token the caret is inside, or `None` when no token is
/// active. See the method docs for the rules.
fn mention_range_at(input: &str, cursor_byte: usize) -> Option<(usize, usize)> {
    if cursor_byte > input.len() {
        return None;
    }
    let before = &input[..cursor_byte];
    // Walk back over chars from the cursor looking for an `@` without
    // crossing whitespace. `char_indices` gives byte offsets so the range we
    // return can be sliced straight out of the input.
    let mut chars_before: Vec<(usize, char)> = before.char_indices().collect();
    while let Some((idx, c)) = chars_before.pop() {
        if c.is_whitespace() {
            return None;
        }
        if c == '@' {
            let preceding_whitespace = chars_before
                .last()
                .map(|(_, prev_c)| prev_c.is_whitespace())
                .unwrap_or(true);
            return if preceding_whitespace {
                Some((idx, cursor_byte))
            } else {
                None
            };
        }
    }
    None
}

#[derive(Clone, Copy)]
pub(crate) struct ModelSolution {
    pub id: &'static str,
    pub name: &'static str,
    pub model: &'static str,
    pub description: &'static str,
    pub custom_endpoint: bool,
    /// Model context window in tokens, used by the header context-usage
    /// indicator. `0` means "unknown" (custom / local / mock), which hides the
    /// indicator rather than showing a meaningless fill level.
    pub context_window: usize,
}

/// Look up the context window (in tokens) for a provider preset id. Returns `0`
/// when the provider is unknown or has no fixed window.
pub(crate) fn model_context_window(provider: &str) -> usize {
    SOLUTIONS
        .iter()
        .find(|s| s.id == provider)
        .map(|s| s.context_window)
        .unwrap_or(0)
}

const SOLUTIONS: &[ModelSolution] = &[
    ModelSolution {
        id: "kimi-code",
        name: "Kimi Code",
        model: "kimi-for-coding",
        description: "Kimi coding subscription (auto-updated model)",
        custom_endpoint: false,
        context_window: 128_000,
    },
    ModelSolution {
        id: "openai",
        name: "OpenAI",
        model: "gpt-4o",
        description: "OpenAI API",
        custom_endpoint: false,
        context_window: 128_000,
    },
    ModelSolution {
        id: "gemini",
        name: "Gemini",
        model: "gemini-1.5-flash",
        description: "Google Gemini",
        custom_endpoint: false,
        context_window: 1_000_000,
    },
    ModelSolution {
        id: "kimi",
        name: "Kimi Platform",
        model: "moonshot-v1-8k",
        description: "Moonshot pay-as-you-go API",
        custom_endpoint: false,
        context_window: 8_000,
    },
    ModelSolution {
        id: "deepseek",
        name: "DeepSeek",
        model: "deepseek-chat",
        description: "DeepSeek AI",
        custom_endpoint: false,
        context_window: 64_000,
    },
    ModelSolution {
        id: "qwen",
        name: "Qwen",
        model: "qwen-plus",
        description: "Alibaba DashScope",
        custom_endpoint: false,
        context_window: 131_072,
    },
    ModelSolution {
        id: "glm",
        name: "GLM",
        model: "glm-4-plus",
        description: "Zhipu AI",
        custom_endpoint: false,
        context_window: 128_000,
    },
    ModelSolution {
        id: "volcengine",
        name: "Volcengine",
        model: "deepseek-v3-250324",
        description: "ByteDance Ark",
        custom_endpoint: false,
        context_window: 64_000,
    },
    ModelSolution {
        id: "llama",
        name: "Llama",
        model: "local-model",
        description: "Local Llama server",
        custom_endpoint: false,
        context_window: 0,
    },
    ModelSolution {
        id: "custom",
        name: "Custom relay",
        model: "custom-model",
        description: "OpenAI-compatible endpoint",
        custom_endpoint: true,
        context_window: 0,
    },
    ModelSolution {
        id: "mock",
        name: "Mock",
        model: "mock-model",
        description: "Test provider",
        custom_endpoint: false,
        context_window: 0,
    },
];

#[derive(PartialEq, Clone, Copy)]
pub enum Modal {
    None,
    Models,
    HistorySearch,
    Permission,
    ApiKey,
    Endpoint,
    ModelName,
    Help,
    Sessions,
    /// Full-output detail overlay for a focused tool step. The step is
    /// identified by `App::tool_detail_message_idx`; `tool_detail_scroll`
    /// holds the overlay's own scroll offset.
    ToolStepDetail,
}

pub struct App {
    pub input: String,
    /// Structured transcript messages (semantic document model).
    pub messages: Vec<TranscriptMessage>,
    pub scroll: u16,
    /// Whether the view follows the newest content (auto-scroll to bottom).
    pub follow_bottom: bool,
    /// Last measured stream height in lines and viewport height, used to pin
    /// the view to the bottom while following.
    pub content_lines: usize,
    pub view_height: u16,
    pub max_scroll: u16,
    /// Expanded step pinned under the HUD bar (its message index + screen rect),
    /// when its body is scrolled into view. Clicks inside the rect collapse it.
    pub sticky_step: Option<usize>,
    pub sticky_rect: Option<ratatui::layout::Rect>,
    /// Screen rect of the goal segment in the hint bar for the current frame,
    /// so clicks inside it route to `/goal status`. `None` when no goal is
    /// shown or the hint bar is hidden (overlay modal open).
    pub hint_goal_rect: Option<ratatui::layout::Rect>,
    /// Content-line index of the sticky step's real header. Used to re-anchor
    /// the scroll offset when the user collapses the pinned step so the header
    /// lands at the top of the viewport instead of jumping to unrelated content.
    pub sticky_header_line: Option<usize>,
    /// Content-line the user asked to keep pinned at the top of the viewport by
    /// collapsing a sticky header. While set, the per-frame scroll clamp is
    /// allowed to scroll past the natural `max_scroll` so a short tail of
    /// content below the collapsed step does not yank the header back down.
    /// Cleared on any manual scroll, view reset, or when auto-follow resumes.
    pub pin_header_line: Option<usize>,
    /// Stack of sub-agent task call-ids that the view is zoomed into. Empty
    /// means the root conversation is shown; a non-empty stack renders the
    /// focused `task` tool step's child messages as the main stream, with a
    /// navigation bar to return to the parent or cycle sibling sub-agents.
    pub focus_stack: Vec<String>,
    pub tx: mpsc::UnboundedSender<AgentRequest>,
    pub should_quit: Arc<AtomicBool>,
    pub suggestion_index: Option<usize>,
    pub custom_commands: Vec<(String, String)>,
    pub cursor_position: usize,
    pub input_scroll: usize,
    pub active_modal: Modal,
    pub modal_index: usize,
    pub current_provider: String,
    pub current_model: String,
    /// Display form of the current working directory, with `$HOME` swapped for
    /// `~`. Captured once at startup because the TUI process never `chdir`s.
    pub cwd_display: String,
    /// Raw current working directory captured at startup. Used to resolve
    /// `@path` mention completions against the real filesystem.
    pub cwd: std::path::PathBuf,
    /// Cached recursive project file listing for `@path` completion, populated
    /// lazily on the first `@` mention and reused afterwards. Mirrors the
    /// per-directory picker cache in opencode's TUI. Invalidated after each
    /// accepted path completion so newly-created files become visible without
    /// a restart. `None` = not scanned yet.
    pub path_scan_cache: Option<PathScan>,
    pub current_goal: Option<Goal>,
    pub loop_status: String,
    pub activity_status: String,
    pub pending_permission: Option<PermissionRequest>,
    /// Rows shown in the sessions picker (`/sessions` or `neenee resume`).
    pub sessions_overview: Vec<SessionOverview>,
    pub permission_confirm_always: bool,
    pub permission_scroll: usize,
    pub permission_max_scroll: usize,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
    /// Images pasted (Ctrl+V) and waiting to be sent with the next message.
    pub pending_images: Vec<ImagePart>,
    /// Semantic selection state.
    pub selection: SelectionState,
    /// Drag gesture state.
    pub drag: SelectionDrag,
    /// Layout map for the current frame (updated each draw).
    pub layout_map: LayoutMap,
    /// Message index of the reasoning trace whose header currently rests under
    /// the mouse pointer (inline or sticky pinned), so the next draw brightens
    /// it as a dark→bright hover affordance hinting that it is clickable.
    /// `None` whenever the pointer is elsewhere or an overlay modal is open.
    pub hovered_reasoning: Option<usize>,
    /// Global tool-step density (false = Compact default, true = Comfortable:
    /// new tool steps spawn expanded). Shared with the response listener.
    pub tool_density: Arc<AtomicBool>,
    /// Message index of the tool step shown in the [`Modal::ToolStepDetail`]
    /// overlay. `None` when the overlay is closed.
    pub tool_detail_message_idx: Option<usize>,
    /// Scroll offset (rows) of the [`Modal::ToolStepDetail`] overlay.
    pub tool_detail_scroll: u16,
    /// Keyboard-focused activatable target in the current frame. Mouse support
    /// is an acceleration path; this is the equivalent keyboard-first path.
    pub focused_target: Option<InteractiveTarget>,
    /// Which surface (input box vs conversation stream) currently owns
    /// keyboard focus. See [`input::FocusZone`] for the full semantics.
    /// Defaults to [`input::FocusZone::Compose`] so typing flows into the
    /// prompt box; `Tab` toggles focus to the stream and back, and any
    /// printable key (or `Esc`) hands it back from Browse.
    pub focus_zone: input::FocusZone,
    /// Tracks the last cursor visibility command we sent to the terminal so
    /// we only emit `Hide` / `Show` escape codes when the desired state
    /// actually changes, avoiding per-frame flicker.
    pub cursor_hidden: bool,
    /// Show a brief "copied" toast. Held until this deadline elapses so the
    /// duration is wall-clock consistent regardless of the event-loop cadence.
    pub copy_toast_until: Option<std::time::Instant>,
    pub copy_toast_message: String,
    pub copy_toast_failed: bool,
    /// Ticks remaining in which a second Ctrl+C quits.
    pub ctrl_c_armed_ticks: u8,
    /// Ticks remaining in which a second Esc interrupts the running task.
    pub esc_armed_ticks: u8,
    /// Monotonic per-frame counter that drives the status bar spinner so the
    /// harness never looks frozen while a turn is in flight.
    pub spinner_tick: usize,
    /// Input stashed while the API-key modal borrows the input line.
    pub stashed_input: String,
    /// Solution index currently being configured.
    pub setup_solution: Option<usize>,
    pub setup_endpoint: Option<String>,
    pub setup_model: Option<String>,
    /// Lowercase provider name → whether a usable API key is configured.
    pub key_status: HashMap<String, bool>,
    /// Theme.
    pub theme: Theme,
    /// MCP server statuses loaded at startup. Mirrored into the header as a
    /// compact right-aligned summary.
    pub mcp_statuses: Vec<(String, McpConnectionStatus)>,
}

struct UiRuntime {
    current_provider: Arc<Mutex<String>>,
    current_model: Arc<Mutex<String>>,
    harness: Arc<Mutex<HarnessSnapshot>>,
    activity_status: Arc<Mutex<String>>,
    pending_permission: Arc<Mutex<Option<PermissionRequest>>>,
    is_responding: Arc<AtomicBool>,
    messages: Arc<Mutex<Vec<TranscriptMessage>>>,
    key_status: Arc<Mutex<HashMap<String, bool>>>,
    /// Sessions picker rows + a one-shot request to open the picker modal.
    sessions_overview: Arc<Mutex<Vec<SessionOverview>>>,
    open_sessions: Arc<AtomicBool>,
}

impl App {
    pub fn byte_cursor(&self) -> usize {
        self.input
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.cursor_position)
            .unwrap_or(self.input.len())
    }

    pub fn cursor_display_x(&self) -> u16 {
        self.input[..self.byte_cursor()].width() as u16
    }

    /// Classify which completion menu, if any, should be shown for the current
    /// input + cursor state. Slash commands take priority over `@path` mentions
    /// because a slash input is a command-in-progress and never carries inline
    /// file references.
    pub fn completion_kind(&self) -> CompletionKind {
        if self.input.starts_with('/') {
            CompletionKind::Slash
        } else if self.active_mention_range().is_some() {
            CompletionKind::Path
        } else {
            CompletionKind::None
        }
    }

    /// Compute the live completion candidates for the current input + cursor.
    /// Returns an empty `Vec` when no menu should be shown. See [`Completion`]
    /// for the slash-vs-path replace-range semantics. Takes `&mut self` so the
    /// `@path` scan can populate [`App::path_scan_cache`] on first use.
    pub fn completions(&mut self) -> Vec<Completion> {
        let current = self.input.to_lowercase();

        // Subcommand completion for /mode
        if let Some(after) = current.strip_prefix("/mode ") {
            return [
                ("/mode build", "Build mode — full read/write tool access"),
                (
                    "/mode plan",
                    "Plan mode — read-only tools, safe exploration",
                ),
            ]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/mode ")
                    .map(|sub| sub.to_lowercase().starts_with(after))
                    .unwrap_or(false)
            })
            .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
            .collect();
        }

        if let Some(after) = current.strip_prefix("/goal ") {
            return [
                ("/goal status", "Show the current goal"),
                ("/goal done", "Mark the current goal completed"),
                ("/goal clear", "Remove the current goal"),
            ]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/goal ")
                    .map(|sub| sub.starts_with(after))
                    .unwrap_or(false)
            })
            .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
            .collect();
        }

        if let Some(after) = current.strip_prefix("/loop ") {
            return [
                ("/loop 8", "Run up to 8 autonomous iterations"),
                ("/loop resume", "Resume an unfinished durable checkpoint"),
                ("/loop status", "Show autonomous loop status"),
                ("/loop stop", "Stop the active loop"),
            ]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/loop ")
                    .map(|sub| sub.starts_with(after))
                    .unwrap_or(false)
            })
            .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
            .collect();
        }

        if let Some(after) = current.strip_prefix("/permissions ") {
            return [(
                "/permissions clear",
                "Clear process-local always-allow rules",
            )]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/permissions ")
                    .map(|sub| sub.starts_with(after))
                    .unwrap_or(false)
            })
            .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
            .collect();
        }

        if let Some(after) = current.strip_prefix("/session ") {
            return [
                ("/session status", "Show session id and loop checkpoint"),
                ("/session list", "List durable session branches"),
                (
                    "/session resume",
                    "Resume the most recent or selected session",
                ),
                ("/session fork", "Fork the current conversation"),
                ("/session new", "Start a new durable session"),
            ]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/session ")
                    .map(|sub| sub.starts_with(after))
                    .unwrap_or(false)
            })
            .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
            .collect();
        }

        if current.starts_with('/') {
            return SLASH_COMMANDS
                .iter()
                .filter(|(cmd, _)| cmd.starts_with(&current))
                .map(|(cmd, desc)| Completion::whole_input(cmd, desc, self.input.len()))
                .chain(self.custom_commands.iter().filter_map(|(command, desc)| {
                    if command.starts_with(&current) {
                        Some(Completion::whole_input(
                            command.as_str(),
                            desc.as_str(),
                            self.input.len(),
                        ))
                    } else {
                        None
                    }
                }))
                .collect();
        }

        // Inline `@path` file mention completion.
        if let Some(range) = self.active_mention_range() {
            return self.enumerate_path_completions(range);
        }

        Vec::new()
    }

    /// Locate the `@mention` token the cursor is currently inside, if any.
    /// Returns the byte range `(start, end)` of the token inclusive of the
    /// leading `@`. A mention only triggers completion when:
    ///
    /// - The `@` is at the start of the input or preceded by whitespace, so it
    ///   is not confused with e.g. `user@example` in pasted prose.
    /// - The cursor sits somewhere inside the `@`-prefixed run, not after a
    ///   whitespace that terminated it.
    /// - The text between `@` and the cursor contains no whitespace.
    pub fn active_mention_range(&self) -> Option<(usize, usize)> {
        mention_range_at(&self.input, self.byte_cursor())
    }

    /// Enumerate filesystem entries that extend the `@path` prefix the cursor
    /// is currently in. `mention_range` is the inclusive `(@..cursor)` byte
    /// range produced by [`Self::active_mention_range`]. Pulls from the cached
    /// recursive project scan (populated on first use) and filters with
    /// [`path_query_match`], so each keystroke only filters — it never touches
    /// the filesystem. Empty descriptions match opencode's minimal aesthetic;
    /// directories are distinguished by their trailing `/` label.
    fn enumerate_path_completions(&mut self, mention_range: (usize, usize)) -> Vec<Completion> {
        let (at_start, cursor_end) = mention_range;
        // Skip the `@` itself — only the path portion is replaced/extended.
        // Clone into an owned String so the borrow on `self.input` ends before
        // we mutably borrow `self` for the cache populate below.
        let after_at = self.input[at_start + 1..cursor_end].to_string();

        // Lazy-populate the cache on first `@` mention; subsequent calls reuse
        // it. `path_scan()` is `&mut self`, so clone the entries out to avoid
        // holding a borrow across the iterator below.
        let entries: Vec<String> = self.path_scan().entries.clone();

        let mut comps: Vec<Completion> = entries
            .iter()
            .filter(|p| path_query_match(p, &after_at))
            .take(MAX_PATH_COMPLETIONS)
            .map(|p| Completion {
                label: p.clone(),
                description: String::new(),
                replace_start: at_start + 1,
                replace_end: cursor_end,
            })
            .collect();
        // path_query_match + scan already sort, but the take() may have
        // shuffled entries between filter passes; re-sort for stability.
        comps.sort_by(|a, b| {
            let a_dir = a.label.ends_with('/');
            let b_dir = b.label.ends_with('/');
            b_dir
                .cmp(&a_dir)
                .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
        });
        comps
    }

    /// Borrow the cached recursive project listing, populating it on first
    /// access. Mirrors opencode's per-directory picker cache: one
    /// [`scan_project_files`] call per App session, then pure filtering.
    fn path_scan(&mut self) -> &PathScan {
        if self.path_scan_cache.is_none() {
            self.path_scan_cache = Some(scan_project_files(&self.cwd));
        }
        self.path_scan_cache.as_ref().unwrap()
    }

    /// Toggle the expansion of the tool step / reasoning trace at `mi`,
    /// keeping its header pinned to the screen position the user interacted with.
    ///
    /// A toggle inserts or removes the body lines that sit *below* the header,
    /// so the header's own content-line never moves. That gives a simple rule
    /// for keeping the header where the user clicked:
    ///
    /// - Visible (in-stream) header: leave `scroll` untouched and the header
    ///   stays on the same row; the body grows or shrinks beneath it.
    /// - Sticky-overlay header (its real header is scrolled off the top): point
    ///   `scroll` at the recorded header content-line so the real header lands
    ///   at row 0 where the overlay sat. The line is also recorded in
    ///   `pin_header_line` so the per-frame clamp does not pull it back down
    ///   once the collapsed body shortens the stream.
    /// - Either way `follow_bottom` is cleared: the user is now pinning their
    ///   attention on this header, so the next frame's auto-follow must not
    ///   yank it away (this is what previously let an expand push the header
    ///   off-screen while the view was following the bottom).
    ///
    /// Returns `true` when a step was actually toggled, so callers can gate
    /// side effects like clearing the text selection.
    fn toggle_step_pinned(&mut self, messages: &mut [TranscriptMessage], mi: usize) -> bool {
        let pinned_to_top = self.sticky_step == Some(mi);
        let sticky_header_line = self.sticky_header_line;
        let toggled = resolve_focused_mut(messages, &self.focus_stack, mi)
            .map(|message| {
                if let Some(expanded) = message.tool_step_expanded() {
                    message.set_tool_step_expanded(!expanded);
                    true
                } else if let Some(expanded) = message.thinking_expanded() {
                    message.set_thinking_expanded(!expanded);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if toggled {
            self.follow_bottom = false;
            if pinned_to_top {
                if let Some(header_line) = sticky_header_line {
                    self.scroll = header_line.min(u16::MAX as usize) as u16;
                    // Remember the line so the per-frame clamp (which runs after
                    // this, once the collapsed body has shrunk the stream) keeps
                    // allowing scroll up to it instead of yanking the header
                    // back down to `max_scroll`.
                    self.pin_header_line = Some(header_line);
                }
            } else {
                // Any other toggle (e.g. expanding) is no longer pinning a
                // collapsed header at the top: drop a stale pin so normal
                // clamping resumes.
                self.pin_header_line = None;
            }
        }
        toggled
    }

    fn visible_interactive_targets(&self) -> Vec<InteractiveTarget> {
        let mut targets = self.layout_map.interactive_targets();
        if let Some(message_idx) = self.sticky_step {
            if let Some(message) = self.focused_messages().get(message_idx) {
                let target = if message.is_thinking() {
                    InteractiveTarget::thinking(message_idx)
                } else if message.is_tool_step() || message.is_subagent_task() {
                    InteractiveTarget::tool_step(message_idx)
                } else {
                    return targets;
                };
                if !targets.contains(&target) {
                    targets.insert(0, target);
                }
            }
        }
        targets
    }

    fn retain_visible_focused_target(&mut self) {
        if self.active_modal != Modal::None {
            self.focused_target = None;
            return;
        }
        if let Some(target) = self.focused_target {
            if !self.visible_interactive_targets().contains(&target) {
                self.focused_target = None;
            }
        }
    }

    fn focus_interactive_target(&mut self, direction: i8) {
        let targets = self.visible_interactive_targets();
        if targets.is_empty() {
            self.focused_target = None;
            return;
        }

        let current = self
            .focused_target
            .and_then(|target| targets.iter().position(|candidate| *candidate == target));
        let next = match (current, direction < 0) {
            (Some(0), true) => targets.len() - 1,
            (Some(idx), true) => idx - 1,
            (Some(idx), false) => (idx + 1) % targets.len(),
            (None, true) => targets.len() - 1,
            (None, false) => 0,
        };

        self.focused_target = Some(targets[next]);
        self.selection = SelectionState::None;
        self.drag.cancel();
    }

    /// Whether the view is currently zoomed into a sub-agent task.
    pub fn in_subagent_view(&self) -> bool {
        !self.focus_stack.is_empty()
    }

    /// The message slice currently in view: the root conversation, or the
    /// focused sub-agent task's child messages.
    pub fn focused_messages(&self) -> &[TranscriptMessage] {
        let Some(call_id) = self.focus_stack.last() else {
            return &self.messages;
        };
        self.messages
            .iter()
            .find_map(|message| {
                if message.is_subagent_task()
                    && message.tool_step_call_id() == Some(call_id.as_str())
                {
                    message.subagent_children()
                } else {
                    None
                }
            })
            .unwrap_or(&[])
    }

    /// Reset transient view state (scroll, selection, sticky pinning) when the
    /// focused message slice changes.
    fn reset_view_state(&mut self) {
        self.scroll = 0;
        self.follow_bottom = true;
        self.selection = SelectionState::None;
        self.drag.cancel();
        self.sticky_step = None;
        self.sticky_rect = None;
        self.sticky_header_line = None;
        self.pin_header_line = None;
        self.focused_target = None;
    }

    /// Zoom into a sub-agent task's child messages.
    pub fn enter_subagent(&mut self, call_id: String) {
        self.focus_stack.push(call_id);
        self.reset_view_state();
    }

    /// Return from the current sub-agent view to its parent. Returns true if a
    /// view was actually popped.
    pub fn exit_subagent(&mut self) -> bool {
        if self.focus_stack.pop().is_some() {
            self.reset_view_state();
            true
        } else {
            false
        }
    }

    /// Cycle to the previous (`dir < 0`) or next (`dir > 0`) sibling sub-agent
    /// task at the current focus level. No-op when not in a sub-agent view or
    /// when there are no siblings.
    pub fn cycle_sibling(&mut self, dir: i8) {
        let Some(current) = self.focus_stack.last().cloned() else {
            return;
        };
        let task_ids: Vec<String> = self
            .messages
            .iter()
            .filter_map(|message| {
                if message.is_subagent_task() {
                    message.tool_step_call_id().map(String::from)
                } else {
                    None
                }
            })
            .collect();
        let Some(idx) = task_ids.iter().position(|id| *id == current) else {
            return;
        };
        if task_ids.len() < 2 {
            return;
        }
        let n = task_ids.len() as isize;
        let next = ((idx as isize + dir as isize).rem_euclid(n)) as usize;
        self.focus_stack.pop();
        self.focus_stack.push(task_ids[next].clone());
        self.reset_view_state();
    }

    /// Fuzzy-filtered view of [`App::input_history`] for the Ctrl+R
    /// (`Modal::HistorySearch`) modal. Returns `(original_index, FuzzyMatch)`
    /// pairs sorted by descending match score, with input order as the stable
    /// tiebreaker so equally-good matches keep their top-to-bottom history
    /// order. Computed from scratch on every call: history is small and this
    /// is invoked at most a few times per frame (modal navigation, Enter
    /// accept, and rendering), so a cached field would just add stale-state
    /// risk for no measurable win.
    pub fn history_filtered(&self) -> Vec<(usize, fuzzy::FuzzyMatch)> {
        let mut ranked = fuzzy::rank(&self.input_history, &self.input);
        fuzzy::sort_by_score(&mut ranked);
        ranked
    }
}

/// Undo raw mode, leave the alternate screen, and turn off mouse tracking.
/// Used both on graceful shutdown and from the signal guard so an externally
/// killed process (e.g. `pkill neenee`) does not strand the terminal in a
/// state where every mouse move spews SGR escape codes into the shell.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    use std::io::Write;
    let _ = stdout.flush();
}

/// Catch termination signals and restore the terminal before exiting. Without
/// this, SIGTERM/SIGHUP (as sent by `pkill`) terminates the process without
/// running `run_tui`'s normal cleanup, leaving the host terminal in raw mode
/// with mouse capture enabled.
fn spawn_signal_guard() {
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut interrupt = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut quit = match signal(SignalKind::quit()) {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
            _ = hangup.recv() => {}
            _ = quit.recv() => {}
        }
        restore_terminal();
        std::process::exit(130);
    });
}

pub async fn run_tui(
    tx: mpsc::UnboundedSender<AgentRequest>,
    mut rx: mpsc::UnboundedReceiver<AgentResponse>,
    initial_provider: String,
    initial_model: String,
    input_history: Vec<String>,
    initial_messages: Vec<Message>,
    custom_commands: Vec<(String, String)>,
    mcp_statuses: Vec<(String, McpConnectionStatus)>,
) -> Result<Vec<String>, Box<dyn Error>> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Request the Kitty enhanced-keyboard protocol so modifier-bearing keys
    // that collide with legacy control bytes (notably Ctrl+M == Enter) are
    // reported distinctly. crossterm only emits the request when the terminal
    // advertises support, so this is a no-op elsewhere.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.show_cursor()?;
    // Install the signal guard after the terminal enters raw mode + alt screen
    // so any later SIGTERM/SIGINT/SIGHUP restores it instead of stranding it.
    spawn_signal_guard();

    let restored = transcript_messages_from_core(initial_messages);
    let messages = Arc::new(Mutex::new(restored));
    let messages_clone = messages.clone();
    let should_quit = Arc::new(AtomicBool::new(false));
    let should_quit_clone = should_quit.clone();

    let cwd_display = format_cwd_display();

    let current_provider = Arc::new(Mutex::new(initial_provider.clone()));
    let current_model = Arc::new(Mutex::new(initial_model.clone()));
    let cp_clone = current_provider.clone();
    let cm_clone = current_model.clone();

    let is_responding = Arc::new(AtomicBool::new(false));
    let ir_clone = is_responding.clone();
    let harness = Arc::new(Mutex::new(HarnessSnapshot {
        mode: AgentMode::Build,
        goal: None,
        loop_status: "idle".to_string(),
    }));
    let harness_clone = harness.clone();
    let activity_status = Arc::new(Mutex::new(String::new()));
    let activity_clone = activity_status.clone();
    let pending_permission = Arc::new(Mutex::new(None::<PermissionRequest>));
    let pending_permission_clone = pending_permission.clone();
    let key_status = Arc::new(Mutex::new(HashMap::<String, bool>::new()));
    let key_status_clone = key_status.clone();
    let sessions_overview = Arc::new(Mutex::new(Vec::<SessionOverview>::new()));
    let sessions_overview_clone = sessions_overview.clone();
    let open_sessions = Arc::new(AtomicBool::new(false));
    let open_sessions_clone = open_sessions.clone();
    // Global tool-step density (true = Comfortable: new tool steps spawn
    // expanded). Shared with the response listener so steps created mid-turn
    // respect the user's last Ctrl+T choice (ADR-0001 Step 8).
    let tool_density = Arc::new(AtomicBool::new(false));
    let tool_density_clone = tool_density.clone();

    // Spawn response listener
    tokio::spawn(async move {
        let mut reasoning_start: Option<std::time::Instant> = None;
        while let Some(resp) = rx.recv().await {
            match resp {
                AgentResponse::Text(t) => {
                    let (provider, model) = attribution(&cp_clone, &cm_clone).await;
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(
                        TranscriptMessage::new(Role::Assistant, t)
                            .with_attribution(provider, model),
                    );
                    ir_clone.store(false, Ordering::SeqCst);
                    activity_clone.lock().await.clear();
                }
                AgentResponse::Activity(status) => {
                    *activity_clone.lock().await = status;
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::StreamStart => {
                    let (provider, model) = attribution(&cp_clone, &cm_clone).await;
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(
                        TranscriptMessage::new(Role::Assistant, "")
                            .with_attribution(provider, model),
                    );
                    ir_clone.store(true, Ordering::SeqCst);
                    *activity_clone.lock().await = "responding".to_string();
                }
                AgentResponse::StreamDelta(delta) => {
                    let mut msgs = messages_clone.lock().await;
                    if let Some(last) = msgs.last_mut() {
                        last.push_stream(&delta);
                    }
                }
                AgentResponse::StreamEnd(final_content) => {
                    ir_clone.store(true, Ordering::SeqCst);
                    *activity_clone.lock().await = "finalizing response".to_string();
                    let mut msgs = messages_clone.lock().await;
                    if let Some(last) = msgs.last_mut() {
                        last.raw = final_content;
                        last.reparse();
                    }
                }
                AgentResponse::StreamDiscard => {
                    let mut msgs = messages_clone.lock().await;
                    if msgs
                        .last()
                        .is_some_and(|message| message.role == Role::Assistant)
                    {
                        msgs.pop();
                    }
                }
                AgentResponse::StreamReasoningDelta(delta) => {
                    let mut msgs = messages_clone.lock().await;
                    if let Some(last) = msgs.last_mut().filter(|message| message.is_thinking()) {
                        last.push_stream(&delta);
                        if let MessageKind::Thinking { content, .. } = &mut last.kind {
                            content.push_str(&delta);
                        }
                    } else {
                        // StreamStart inserts an empty assistant placeholder before
                        // the first reasoning delta. Reasoning renders as its own
                        // reasoning trace, so that placeholder is never used and only
                        // leaves an extra blank line between the user message and the
                        // reasoning header. Drop it before creating the reasoning trace
                        // so restored history and live reasoning have identical
                        // spacing.
                        if msgs
                            .last()
                            .is_some_and(|m| m.role == Role::Assistant && m.raw.is_empty())
                        {
                            msgs.pop();
                        }
                        let (provider, model) = attribution(&cp_clone, &cm_clone).await;
                        msgs.push(
                            TranscriptMessage::thinking(delta).with_attribution(provider, model),
                        );
                        reasoning_start = Some(std::time::Instant::now());
                    }
                }
                AgentResponse::StreamReasoningEnd(content) => {
                    let duration_ms = reasoning_start
                        .take()
                        .map(|started| started.elapsed().as_millis() as u64);
                    let mut msgs = messages_clone.lock().await;
                    // The round closes with `AssistantEnd` *before* `ReasoningEnd`
                    // (see golden_reasoning_precedes_text_in_the_same_round), so by
                    // the time this arrives the assistant's text message is usually
                    // the literal last message. Scan backward for the most recent
                    // Thinking message that is still streaming (`duration_ms: None`)
                    // instead of relying on it being last — otherwise the trace's
                    // duration never gets stamped and the spinner runs forever.
                    let target = msgs.iter_mut().rfind(|message| {
                        matches!(
                            &message.kind,
                            MessageKind::Thinking {
                                duration_ms: None,
                                ..
                            }
                        )
                    });
                    if let Some(last) = target {
                        last.raw = content.clone();
                        last.reparse();
                        if let MessageKind::Thinking {
                            content: current,
                            duration_ms: d,
                            ..
                        } = &mut last.kind
                        {
                            *current = content;
                            if d.is_none() {
                                *d = Some(duration_ms.unwrap_or(0));
                            }
                        }
                    }
                }
                AgentResponse::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    *activity_clone.lock().await = tool_activity_status(&name).to_string();
                    let (provider, model) = attribution(&cp_clone, &cm_clone).await;
                    let mut msgs = messages_clone.lock().await;
                    let mut message =
                        TranscriptMessage::tool_step(id, name, arguments).with_attribution(provider, model);
                    // Respect the global density: in Comfortable mode new tool
                    // steps spawn expanded so mid-turn calls match the user's
                    // last Ctrl+T choice.
                    if tool_density_clone.load(Ordering::SeqCst) {
                        message.set_tool_step_expanded(true);
                    }
                    msgs.push(message);
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::ToolResult {
                    id,
                    name,
                    output,
                    structured,
                    duration_ms,
                } => {
                    *activity_clone.lock().await = "thinking".to_string();
                    let (provider, model) = attribution(&cp_clone, &cm_clone).await;
                    let mut msgs = messages_clone.lock().await;
                    if !msgs.iter_mut().any(|message| {
                        message.finish_tool_step(&id, output.clone(), structured.clone(), duration_ms)
                    }) {
                        let mut message =
                            TranscriptMessage::tool_step(id.clone(), name.clone(), "{}")
                                .with_attribution(provider, model);
                        message.finish_tool_step(&id, output, structured, duration_ms);
                        msgs.push(message);
                    }
                }
                AgentResponse::ToolCancelled { id, .. } => {
                    // Convergence: an in-flight call was aborted by an
                    // interrupt. Flip its step (and any nested sub-agent
                    // children) to Cancelled so it never stays "running".
                    let mut msgs = messages_clone.lock().await;
                    if !msgs.iter_mut().any(|message| message.cancel_tool_step(&id)) {
                        // The ToolCall event may have been dropped with the
                        // aborted turn; synthesize a minimal cancelled step so
                        // the user still sees the call was abandoned.
                        let mut message =
                            TranscriptMessage::tool_step(id.clone(), "tool", "{}");
                        message.cancel_tool_step(&id);
                        msgs.push(message);
                    }
                }
                AgentResponse::ToolStream { id, stream } => {
                    // Live partial output from a running tool (e.g. bash
                    // stdout). Accumulate into the running step so it updates
                    // in place instead of freezing on a spinner.
                    let mut msgs = messages_clone.lock().await;
                    if !msgs.iter_mut().any(|message| message.push_tool_stream(&id, &stream)) {
                        // Unknown id: drop silently — the matching ToolCall may
                        // have been dropped with an aborted turn.
                    }
                }
                AgentResponse::SubTask {
                    parent_call_id,
                    event,
                } => {
                    let mut msgs = messages_clone.lock().await;
                    if let Some(message) = msgs
                        .iter_mut()
                        .find(|m| m.is_tool_step() && matches!(&m.kind, crate::document::MessageKind::ToolStep { id, .. } if id == &parent_call_id))
                    {
                        message.push_subtask_event(&event);
                    }
                }
                AgentResponse::PermissionRequest(request) => {
                    *pending_permission_clone.lock().await = Some(request);
                    *activity_clone.lock().await = "awaiting permission".to_string();
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::PermissionsCleared => {
                    *pending_permission_clone.lock().await = None;
                    activity_clone.lock().await.clear();
                }
                AgentResponse::ProviderKeys(status) => {
                    *key_status_clone.lock().await = status.into_iter().collect();
                }
                AgentResponse::ConversationCleared => {
                    messages_clone.lock().await.clear();
                }
                AgentResponse::ConversationReplaced(messages) => {
                    *messages_clone.lock().await = transcript_messages_from_core(messages);
                }
                AgentResponse::SessionsOverview(sessions) => {
                    *sessions_overview_clone.lock().await = sessions;
                    open_sessions_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::Compacted {
                    archived_messages,
                    before_chars,
                    after_chars,
                } => {
                    messages_clone.lock().await.push(TranscriptMessage::new(
                        Role::System,
                        format!(
                            "Compacted {} messages: {} -> {} chars.",
                            archived_messages, before_chars, after_chars
                        ),
                    ));
                }
                AgentResponse::HarnessState(snapshot) => {
                    let running = snapshot.loop_status != "idle";
                    *harness_clone.lock().await = snapshot;
                    ir_clone.store(running, Ordering::SeqCst);
                    if !running {
                        activity_clone.lock().await.clear();
                    }
                }
                AgentResponse::GoalUpdated(goal) => {
                    harness_clone.lock().await.goal = Some(goal);
                }
                AgentResponse::ModeChanged(mode) => {
                    harness_clone.lock().await.mode = mode;
                }
                AgentResponse::RetryScheduled {
                    attempt,
                    max_attempts,
                    delay_ms,
                    message,
                } => {
                    let seconds = delay_ms.div_ceil(1_000);
                    *activity_clone.lock().await = format!(
                        "retry {}/{} in {}s · {}",
                        attempt,
                        max_attempts,
                        seconds,
                        compact_retry_reason(&message)
                    );
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::Error(e) => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(TranscriptMessage::new(
                        Role::System,
                        format!("Error: {}", e),
                    ));
                    ir_clone.store(false, Ordering::SeqCst);
                    activity_clone.lock().await.clear();
                }
                AgentResponse::Exit => {
                    should_quit_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::ProviderSwitched { provider, model } => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(TranscriptMessage::new(
                        Role::System,
                        format!("System: Provider switched to {} ({})", provider, model),
                    ));
                    *cp_clone.lock().await = provider;
                    *cm_clone.lock().await = model;
                }
            }
        }
    });

    let messages_for_loop = messages.clone();

    let mut app = App {
        input: String::new(),
        messages: Vec::new(),
        scroll: 0,
        follow_bottom: true,
        content_lines: 0,
        view_height: 0,
        max_scroll: 0,
        sticky_step: None,
        sticky_rect: None,
        hint_goal_rect: None,
        sticky_header_line: None,
        pin_header_line: None,
        focus_stack: Vec::new(),
        tx,
        should_quit,
        suggestion_index: None,
        custom_commands,
        cursor_position: 0,
        input_scroll: 0,
        active_modal: Modal::None,
        modal_index: 0,
        current_provider: initial_provider,
        current_model: initial_model,
        cwd_display,
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        path_scan_cache: None,
        current_goal: None,
        loop_status: "idle".to_string(),
        activity_status: String::new(),
        pending_permission: None,
        sessions_overview: Vec::new(),
        permission_confirm_always: false,
        permission_scroll: 0,
        permission_max_scroll: 0,
        input_history,
        history_index: None,
        pending_images: Vec::new(),
        selection: SelectionState::None,
        drag: SelectionDrag::default(),
        layout_map: LayoutMap::new(),
        hovered_reasoning: None,
        tool_density: tool_density.clone(),
        tool_detail_message_idx: None,
        tool_detail_scroll: 0,
        focused_target: None,
        focus_zone: input::FocusZone::Compose,
        cursor_hidden: false,
        copy_toast_until: None,
        copy_toast_message: String::new(),
        copy_toast_failed: false,
        ctrl_c_armed_ticks: 0,
        esc_armed_ticks: 0,
        spinner_tick: 0,
        stashed_input: String::new(),
        setup_solution: None,
        setup_endpoint: None,
        setup_model: None,
        key_status: HashMap::new(),
        theme: Theme::default(),
        mcp_statuses,
    };

    // Run app
    let res = run_app_loop(
        &mut terminal,
        &mut app,
        UiRuntime {
            current_provider,
            current_model,
            harness,
            activity_status,
            pending_permission,
            is_responding,
            messages: messages_for_loop,
            key_status,
            sessions_overview,
            open_sessions,
        },
    )
    .await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        return Err(err.into());
    }

    Ok(app.input_history)
}

/// Build a display form of the process working directory with the user's home
/// directory replaced by `~`. Falls back to the raw path (or `"."` if the cwd
/// cannot be read) when home detection fails.
fn format_cwd_display() -> String {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return ".".to_string(),
    };
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = cwd.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rel.display());
        }
    }
    cwd.display().to_string()
}

async fn run_app_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    runtime: UiRuntime,
) -> io::Result<()> {
    let mut _copy_toast_timer: u8 = 0;
    // Clipboard copies run in background tasks so a slow/hanging system
    // clipboard (arboard/wl-copy) can never freeze the event loop.
    let (copy_tx, mut copy_rx) =
        mpsc::unbounded_channel::<Result<clipboard::CopyOutcome, String>>();
    // Number of clipboard copies still in flight. While this is non-zero the
    // event loop uses a short poll interval so the "copied" toast appears
    // within ~16ms of completion instead of waiting up to the full idle tick.
    let copy_pending = Arc::new(AtomicUsize::new(0));

    // Clipboard paste reads (Ctrl+V) run in background tasks for the same
    // reason: arboard/wl-paste must never block the event loop.
    let (paste_tx, mut paste_rx) = mpsc::unbounded_channel::<clipboard::ClipboardRead>();

    loop {
        if app.should_quit.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Apply any completed background clipboard copies.
        while let Ok(result) = copy_rx.try_recv() {
            set_copy_feedback(app, result);
            app.copy_toast_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(1800));
        }

        // Apply any completed clipboard paste reads.
        while let Ok(read) = paste_rx.try_recv() {
            apply_clipboard_paste(app, read);
        }

        // Sync provider/model from listener
        {
            app.current_provider = runtime.current_provider.lock().await.clone();
            app.current_model = runtime.current_model.lock().await.clone();
            let harness = runtime.harness.lock().await.clone();
            app.current_goal = harness.goal;
            app.loop_status = harness.loop_status;
            app.activity_status = runtime.activity_status.lock().await.clone();
            app.pending_permission = runtime.pending_permission.lock().await.clone();
            app.key_status = runtime.key_status.lock().await.clone();
            if app.pending_permission.is_some() && app.active_modal == Modal::None {
                app.active_modal = Modal::Permission;
                app.modal_index = 0;
                app.permission_scroll = 0;
            } else if app.pending_permission.is_none() && app.active_modal == Modal::Permission {
                app.active_modal = Modal::None;
                app.modal_index = 0;
                app.permission_confirm_always = false;
                app.permission_scroll = 0;
                app.permission_max_scroll = 0;
            }
            // Sessions picker: refresh rows and open the modal on request.
            app.sessions_overview = runtime.sessions_overview.lock().await.clone();
            if runtime.open_sessions.swap(false, Ordering::SeqCst)
                && app.active_modal != Modal::Permission
            {
                app.active_modal = Modal::Sessions;
                app.modal_index = 0;
            }
        }

        // Decrement toast timers
        if let Some(until) = app.copy_toast_until {
            if std::time::Instant::now() >= until {
                app.copy_toast_until = None;
            }
        }
        // While images are staged for the next message, keep a persistent
        // indicator visible so the user knows Enter will send them.
        if !app.pending_images.is_empty() {
            let n = app.pending_images.len();
            app.copy_toast_message = format!(
                "{n} image{} attached — enter to send",
                if n == 1 { "" } else { "s" }
            );
            app.copy_toast_failed = false;
            app.copy_toast_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(600));
        }
        if app.ctrl_c_armed_ticks > 0 {
            app.ctrl_c_armed_ticks -= 1;
        }
        // The Esc armed toast only makes sense while a task is running; once
        // the turn finishes there is nothing left to interrupt, so let it
        // expire immediately rather than mislead the user.
        if app.esc_armed_ticks > 0 {
            if runtime.is_responding.load(Ordering::SeqCst) {
                app.esc_armed_ticks -= 1;
            } else {
                app.esc_armed_ticks = 0;
            }
        }

        // Pull messages from the shared lock into app state for rendering
        app.messages = runtime.messages.lock().await.clone();

        // While following, keep the newest content in view using the previous
        // frame's measurement (max_scroll is recomputed after each draw).
        if app.follow_bottom {
            app.scroll = app.max_scroll;
        }

        // Advance the status-bar spinner phase for this frame. The draw call
        // only reads it, so a single wrapping increment per frame gives a
        // smooth ~10 fps braille animation tied to the 100ms event poll.
        app.spinner_tick = app.spinner_tick.wrapping_add(1);

        // Draw frame
        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            let status = display_status(
                &app.loop_status,
                &app.activity_status,
                app.pending_permission.is_some(),
            );

            // Compute the displayed input text first so the transcript layout can
            // reserve the right height for a wrapping, growing input box.
            let masked_input = if app.active_modal == Modal::ApiKey {
                "•".repeat(app.input.chars().count())
            } else {
                app.input.clone()
            };

            // Overlay modals (Models, Sessions, Help, Permission) replace the
            // entire chrome. Modals that type into the input line keep it.
            let chrome_hidden = !matches!(
                app.active_modal,
                Modal::None
                    | Modal::ApiKey
                    | Modal::Endpoint
                    | Modal::ModelName
                    | Modal::HistorySearch
            );

            // When zoomed into a sub-agent, render its child messages and show
            // a navigation bar; otherwise render the root conversation.
            let view_messages = app.focused_messages();
            let subagent_bar = app.focus_stack.last().and_then(|current| {
                let tasks: Vec<&TranscriptMessage> = app
                    .messages
                    .iter()
                    .filter(|message| message.is_subagent_task())
                    .collect();
                let idx = tasks
                    .iter()
                    .position(|message| message.tool_step_call_id() == Some(current.as_str()))?;
                Some(render::SubagentBarInfo {
                    label: tasks.get(idx)?.subagent_label(),
                    index: idx + 1,
                    total: tasks.len(),
                })
            });

            let transcript_render = render::draw_transcript(
                f,
                &mut layout_map,
                render::TranscriptView {
                    messages: view_messages,
                    scroll: app.scroll,
                    selection: &app.selection,
                    activity: &status,
                    spinner_phase: app.spinner_tick,
                    input: &masked_input,
                    byte_cursor: app.byte_cursor(),
                    chrome_hidden,
                    subagent_bar,
                    // Suppress the hover affordance whenever a modal is open so
                    // no stale highlight bleeds through an overlay.
                    hovered_reasoning: (app.active_modal == Modal::None)
                        .then_some(app.hovered_reasoning)
                        .flatten(),
                    focused_target: (app.active_modal == Modal::None)
                        .then_some(app.focused_target)
                        .flatten(),
                    theme: &app.theme,
                },
            );
            let input_rect = transcript_render.input_rect;
            let hint_rect = transcript_render.hint_rect;
            let content_lines = transcript_render.content_lines;
            let view_height = transcript_render.view_height;
            let sticky = transcript_render.sticky;

            // The hint bar (workspace / model / goal / MCP / context) lives
            // directly below the input box and carries the info the old top
            // header showed. Rendered only when the chrome is visible. It is
            // drawn before the composer because it borrows `view_messages`
            // (an immutable borrow of `app`) while `draw_composer` needs a
            // mutable borrow of `app.input_scroll`.
            let hint_goal_rect = if !chrome_hidden && hint_rect.height > 0 {
                render::draw_hint_bar(
                    f,
                    hint_rect,
                    render::HintBarView {
                        cwd: &app.cwd_display,
                        current_provider: &app.current_provider,
                        current_model: &app.current_model,
                        current_goal: app.current_goal.as_ref(),
                        messages: view_messages,
                        mcp_statuses: &app.mcp_statuses,
                        focus_zone: app.focus_zone,
                    },
                    &app.theme,
                )
                .goal_rect
            } else {
                None
            };

            // The input box is only shown when no overlay modal is open. The
            // `focused` flag drops the panel to its dim "blurred" palette and
            // hides the caret whenever keyboard focus is on the conversation
            // stream (Browse zone), so the user can see at a glance which
            // surface the next keypress will land on.
            if !chrome_hidden {
                let compose_focused = app.focus_zone.is_compose();
                render::draw_composer(
                    f,
                    input_rect,
                    &masked_input,
                    app.byte_cursor(),
                    compose_focused,
                    &app.theme,
                    &mut layout_map,
                    app.active_modal != Modal::ApiKey,
                    &mut app.input_scroll,
                );
            }

            // Now that `view_messages` is no longer borrowed, persist the
            // per-frame layout state back onto `app` for the next iteration
            // and for click routing.
            app.content_lines = content_lines;
            app.view_height = view_height;
            app.hint_goal_rect = hint_goal_rect;
            match sticky {
                Some(info) => {
                    app.sticky_step = Some(info.message_idx);
                    app.sticky_rect = Some(info.rect);
                    app.sticky_header_line = Some(info.header_line);
                }
                None => {
                    app.sticky_step = None;
                    app.sticky_rect = None;
                    app.sticky_header_line = None;
                }
            }

            // Completion menu: slash commands or `@path` file mentions.
            if app.active_modal == Modal::None && app.completion_kind() != CompletionKind::None {
                let completions = app.completions();
                if !completions.is_empty() {
                    render::draw_completion_menu(
                        f,
                        &mut layout_map,
                        &completions,
                        app.suggestion_index,
                        input_rect,
                        &app.theme,
                    );
                }
            }

            // Modals
            match app.active_modal {
                Modal::Models => {
                    render::draw_models_modal(
                        f,
                        &mut layout_map,
                        SOLUTIONS,
                        &app.current_provider,
                        app.modal_index,
                        &app.key_status,
                        &app.theme,
                    );
                }
                Modal::HistorySearch => {
                    let ranked = app.history_filtered();
                    render::draw_history_modal(
                        f,
                        &mut layout_map,
                        &app.input_history,
                        &app.input,
                        &ranked,
                        app.modal_index,
                        &app.theme,
                    );
                }
                Modal::Permission => {
                    if let Some(request) = app.pending_permission.as_ref() {
                        let max_scroll = render::draw_permission_sheet(
                            f,
                            request,
                            app.modal_index,
                            app.permission_confirm_always,
                            app.permission_scroll,
                            &app.theme,
                        );
                        app.permission_max_scroll = max_scroll;
                        app.permission_scroll =
                            app.permission_scroll.min(app.permission_max_scroll);
                    }
                }
                Modal::ApiKey => {
                    let solution = app
                        .setup_solution
                        .and_then(|idx| SOLUTIONS.get(idx))
                        .map(|solution| solution.name)
                        .unwrap_or("provider");
                    render::draw_api_key_modal(f, solution, &masked_input, &app.theme);
                }
                Modal::Endpoint => render::draw_solution_input_modal(
                    f,
                    " Relay endpoint",
                    "Full OpenAI-compatible chat completions URL",
                    &app.input,
                    false,
                    &app.theme,
                ),
                Modal::ModelName => render::draw_solution_input_modal(
                    f,
                    " Model ID",
                    "Model name sent in the request body",
                    &app.input,
                    false,
                    &app.theme,
                ),
                Modal::Help => render::draw_help_modal(f, &app.theme),
                Modal::ToolStepDetail => {
                    if let Some(msg) = app
                        .tool_detail_message_idx
                        .and_then(|idx| app.messages.get(idx))
                    {
                        render::draw_tool_step_detail_overlay(
                            f,
                            msg,
                            app.tool_detail_scroll,
                            &app.theme,
                        );
                    }
                }
                Modal::Sessions => render::draw_sessions_modal(
                    f,
                    &app.sessions_overview,
                    app.modal_index
                        .min(app.sessions_overview.len().saturating_sub(1)),
                    &app.theme,
                ),
                Modal::None => {}
            }

            // Copy toast
            if app.copy_toast_until.is_some() {
                render::draw_copy_toast(
                    f,
                    &app.copy_toast_message,
                    app.copy_toast_failed,
                    &app.theme,
                );
            }
            if app.ctrl_c_armed_ticks > 0 {
                render::draw_armed_toast(f, "press Ctrl+C again to exit", &app.theme);
            }
            if app.esc_armed_ticks > 0 {
                render::draw_armed_toast(f, "press Esc again to interrupt", &app.theme);
            }

            app.layout_map = layout_map;
        })?;

        // Cursor visibility follows the focus zone so the caret only shows up
        // where keys actually land. While a modal is open the modal itself
        // owns the caret (and may hide it for non-edit modals like Help); in
        // Browse zone the input box is blurred so the caret is hidden too.
        // Toggled only when the desired state changes to avoid spamming the
        // terminal with redundant escape codes every frame.
        let cursor_should_hide = app.active_modal == Modal::None
            && app.focus_zone.is_browse();
        if cursor_should_hide != app.cursor_hidden {
            if cursor_should_hide {
                let _ = terminal.hide_cursor();
            } else {
                let _ = terminal.show_cursor();
            }
            app.cursor_hidden = cursor_should_hide;
        }

        // Recompute the bottom scroll offset for the next frame and keep the
        // manual scroll position within bounds when not following.
        let natural_max = app.content_lines.saturating_sub(app.view_height as usize) as u16;
        // `app.max_scroll` stays at the natural bottom so scroll shortcuts
        // (ScrollBottom / wheel down) still land on the real last page.
        app.max_scroll = natural_max;
        if !app.follow_bottom {
            // A collapsed sticky header may leave too little content below it
            // for `natural_max` to reach the header line; while a pin is
            // active, allow scrolling up to that line so the header stays at
            // the top of the viewport instead of being dragged back down.
            let limit = app
                .pin_header_line
                .map(|line| natural_max.max(line.min(u16::MAX as usize) as u16))
                .unwrap_or(natural_max);
            app.scroll = app.scroll.min(limit);
        }
        app.retain_visible_focused_target();

        // Drain all currently-ready input events before redrawing. The first
        // event blocks for the normal poll interval; any further events the
        // terminal has already queued are coalesced with non-blocking polls
        // so they share a single redraw. Without this, pasting text triggers
        // one full screen redraw per pasted character.
        //
        // While a clipboard copy is in flight, shorten the idle poll so the
        // "copied" toast shows within ~16ms of the copy finishing.
        let mut events_drained = false;
        'event_batch: loop {
            let timeout = if events_drained {
                std::time::Duration::ZERO
            } else if copy_pending.load(Ordering::SeqCst) > 0 {
                std::time::Duration::from_millis(16)
            } else {
                std::time::Duration::from_millis(100)
            };
            if !event::poll(timeout)? {
                break;
            }
            events_drained = true;
            let event = event::read()?;
            // The Ctrl+R history-search modal borrows the input line as its
            // fuzzy query, so a literal `/foo` query must NOT trigger the slash
            // completion popup (or `@path` mentions). Suppress completions
            // entirely while that modal is open.
            let suppress_completions = app.active_modal == Modal::HistorySearch;
            // Pre-compute completion data to avoid borrow conflicts with process_event.
            let completions = if suppress_completions {
                Vec::new()
            } else {
                app.completions()
            };
            let suggestion_count = completions.len();
            // The "exact match" auto-accept on Enter only makes sense for slash
            // commands: there, typing an unambiguous prefix and pressing Enter
            // should expand to the unique command rather than send `/go` as a
            // (rejected) command. Path mentions are accepted only via Tab so
            // plain Enter still ships the message as the user typed it.
            let has_exact_suggestion = completions
                .iter()
                .any(|c| c.replace_start == 0 && c.replace_end == app.input.len() && c.label == app.input);
            let completion_kind = if suppress_completions {
                crate::CompletionKind::None
            } else {
                app.completion_kind()
            };
            let in_subagent_view = app.in_subagent_view();
            let action = input::process_event(
                event,
                &mut app.input,
                &mut app.cursor_position,
                input::InputContext {
                    active_modal: app.active_modal,
                    is_responding: runtime.is_responding.load(Ordering::SeqCst),
                    completion_kind,
                    suggestion_count,
                    has_exact_suggestion,
                    suggestion_index: app.suggestion_index,
                    permission_confirm_always: app.permission_confirm_always,
                    in_subagent_view,
                    has_focused_target: app.focused_target.is_some(),
                    focus_zone: app.focus_zone,
                },
                &mut app.drag,
            );
            if !app.input.is_empty() {
                app.focused_target = None;
                // Non-empty input implies the user is composing; make the zone
                // match so key bindings resolve to the input box.
                app.focus_zone = input::FocusZone::Compose;
            }

            match action {
                input::InputAction::None => {}
                input::InputAction::Quit => return Ok(()),
                input::InputAction::SendChat(text) => {
                    // Note: history-search selection no longer flows through
                    // here — Enter in `Modal::HistorySearch` emits the dedicated
                    // `HistoryInsert` action so the chosen entry lands in the
                    // input box for editing instead of being sent immediately.
                    app.active_modal = Modal::None;
                    app.suggestion_index = None;
                    app.input_scroll = 0;

                    // Take any images staged by Ctrl+V so they ship with this
                    // message and are cleared whether or not there is text.
                    let images = std::mem::take(&mut app.pending_images);
                    let has_images = !images.is_empty();

                    if !text.is_empty() || has_images {
                        runtime.is_responding.store(true, Ordering::SeqCst);
                        *runtime.activity_status.lock().await = "queued".to_string();
                        runtime
                            .messages
                            .lock()
                            .await
                            .push(TranscriptMessage::new(Role::User, text.clone()));
                        if !text.is_empty() && app.input_history.last() != Some(&text) {
                            app.input_history.push(text.clone());
                        }
                        app.history_index = None;
                        app.follow_bottom = true;
                        app.pin_header_line = None;
                        let _ = app.tx.send(AgentRequest::Chat { text, images });
                    } else if let Some((start, end)) = app.selection.normalized_range() {
                        // Enter on a selected step: navigate into a sub-agent
                        // task, otherwise toggle that step's expansion.
                        if start.message_idx == end.message_idx {
                            let mi = start.message_idx;
                            let mut messages = runtime.messages.lock().await;
                            // A sub-agent task navigates into its view instead
                            // of expanding.
                            let enter_id = resolve_focused_mut(&mut messages, &app.focus_stack, mi)
                                .and_then(|message| {
                                    if message.is_subagent_task() {
                                        message.tool_step_call_id().map(String::from)
                                    } else {
                                        None
                                    }
                                });
                            if let Some(id) = enter_id {
                                drop(messages);
                                app.enter_subagent(id);
                            } else {
                                let toggled = app.toggle_step_pinned(&mut messages, mi);
                                drop(messages);
                                if toggled {
                                    app.selection = SelectionState::None;
                                }
                            }
                        }
                    }
                }
                input::InputAction::SendSlash(cmd) => {
                    app.suggestion_index = None;
                    app.input_scroll = 0;
                    runtime.is_responding.store(true, Ordering::SeqCst);
                    *runtime.activity_status.lock().await = "queued".to_string();
                    app.follow_bottom = true;
                    app.pin_header_line = None;
                    runtime
                        .messages
                        .lock()
                        .await
                        .push(TranscriptMessage::new(Role::User, cmd.clone()));
                    if app.input_history.last() != Some(&cmd) {
                        app.input_history.push(cmd.clone());
                    }
                    app.history_index = None;
                    let _ = app.tx.send(AgentRequest::SlashCommand(cmd));
                }
                input::InputAction::SwitchProvider { .. } => {
                    if app.active_modal == Modal::Models {
                        let solution = SOLUTIONS[app.modal_index];
                        if solution.custom_endpoint {
                            app.setup_solution = Some(app.modal_index);
                            app.setup_endpoint = None;
                            app.setup_model = None;
                            app.stashed_input = std::mem::take(&mut app.input);
                            app.cursor_position = 0;
                            app.active_modal = Modal::Endpoint;
                        } else if app.key_status.get(solution.id).copied().unwrap_or(true) {
                            let _ = app.tx.send(AgentRequest::SwitchProvider {
                                provider_type: solution.id.to_string(),
                                model: solution.model.to_string(),
                                api_key: None,
                                base_url: None,
                            });
                            app.active_modal = Modal::None;
                        } else {
                            app.setup_solution = Some(app.modal_index);
                            app.stashed_input = std::mem::take(&mut app.input);
                            app.cursor_position = 0;
                            app.active_modal = Modal::ApiKey;
                        }
                    }
                }
                input::InputAction::Interrupt => {
                    // Mirror Ctrl+C's quit pattern: the first Esc only arms a
                    // ~2s window (and shows a toast); the second Esc within
                    // that window actually interrupts the running task.
                    if app.esc_armed_ticks > 0 {
                        app.esc_armed_ticks = 0;
                        let _ = app.tx.send(AgentRequest::Interrupt);
                    } else {
                        app.esc_armed_ticks = 20;
                    }
                }
                input::InputAction::OpenModels => {
                    app.active_modal = Modal::Models;
                    if let Some(idx) = SOLUTIONS
                        .iter()
                        .position(|solution| solution.id == app.current_provider)
                    {
                        app.modal_index = idx;
                    }
                    app.suggestion_index = None;
                }
                input::InputAction::OpenHistory => {
                    // Stash whatever the user was composing so Esc restores it
                    // unchanged; the input box is reused as the fuzzy query
                    // while the modal is open (mirrors the ApiKey / Endpoint /
                    // ModelName modals that also borrow the input line).
                    app.stashed_input = std::mem::take(&mut app.input);
                    app.cursor_position = 0;
                    app.input_scroll = 0;
                    app.suggestion_index = None;
                    app.active_modal = Modal::HistorySearch;
                    // Default to the most-recent entry so an immediate Enter
                    // re-inserts the last-typed item. Empty history → 0.
                    app.modal_index = app.input_history.len().saturating_sub(1);
                }
                input::InputAction::HistoryInsert => {
                    // Enter inside the Ctrl+R modal: pull the highlighted fuzzy
                    // match out of the filtered list and drop it into the input
                    // box for further editing / sending. The message is not
                    // shipped here — the user hits Enter again to send.
                    let ranked = app.history_filtered();
                    let pick = ranked.get(app.modal_index).or_else(|| ranked.first());
                    if let Some((orig_idx, _)) = pick {
                        let original = *orig_idx;
                        app.input = app.input_history[original].clone();
                        app.cursor_position = app.input.chars().count();
                    }
                    // The selection replaces the in-progress draft, so the
                    // stash is dropped (not restored).
                    app.stashed_input.clear();
                    app.input_scroll = 0;
                    app.suggestion_index = None;
                    app.modal_index = 0;
                    app.active_modal = Modal::None;
                }
                input::InputAction::OpenCommands => {
                    // Command palette: seed the input with "/" so the existing
                    // slash-suggestion popup acts as a filterable palette.
                    if !app.input.starts_with('/') {
                        app.input = "/".to_string();
                        app.cursor_position = app.input.chars().count();
                    }
                    app.suggestion_index = None;
                }
                input::InputAction::OpenHelp => {
                    app.active_modal = Modal::Help;
                    app.modal_index = 0;
                }
                input::InputAction::OpenSelectedSession => {
                    if let Some(session) = app.sessions_overview.get(
                        app.modal_index
                            .min(app.sessions_overview.len().saturating_sub(1)),
                    ) {
                        let id = session.id.clone();
                        app.active_modal = Modal::None;
                        app.modal_index = 0;
                        let _ = app
                            .tx
                            .send(AgentRequest::SlashCommand(format!("/session open {}", id)));
                    }
                }
                input::InputAction::DeleteSelectedSession => {
                    if let Some(session) = app.sessions_overview.get(
                        app.modal_index
                            .min(app.sessions_overview.len().saturating_sub(1)),
                    ) {
                        let id = session.id.clone();
                        let _ = app.tx.send(AgentRequest::DeleteSession { id });
                    }
                }
                input::InputAction::CloseModal => {
                    if matches!(
                        app.active_modal,
                        Modal::ApiKey | Modal::Endpoint | Modal::ModelName
                    ) {
                        app.input = std::mem::take(&mut app.stashed_input);
                        app.cursor_position = app.input.chars().count();
                        app.setup_solution = None;
                        app.setup_endpoint = None;
                        app.setup_model = None;
                    } else if app.active_modal == Modal::HistorySearch {
                        // The input box was borrowed as the fuzzy query; hand
                        // the in-progress draft back so Esc is a true cancel.
                        app.input = std::mem::take(&mut app.stashed_input);
                        app.cursor_position = app.input.chars().count();
                        app.input_scroll = 0;
                        app.suggestion_index = None;
                        app.modal_index = 0;
                    }
                    if app.active_modal == Modal::ToolStepDetail {
                        app.tool_detail_message_idx = None;
                        app.tool_detail_scroll = 0;
                    }
                    app.active_modal = Modal::None;
                }
                input::InputAction::ScrollUp => {
                    if app.active_modal == Modal::ToolStepDetail {
                        app.tool_detail_scroll = app.tool_detail_scroll.saturating_sub(1);
                    } else if app.active_modal == Modal::Permission {
                        app.permission_scroll = app.permission_scroll.saturating_sub(4);
                    } else {
                        app.follow_bottom = false;
                        app.pin_header_line = None;
                        // Mouse wheel tick = 4 lines, not 1, so scrolling feels fast
                        // and responsive instead of crawling line-by-line.
                        app.scroll = app.scroll.saturating_sub(4);
                    }
                }
                input::InputAction::ScrollDown => {
                    if app.active_modal == Modal::ToolStepDetail {
                        app.tool_detail_scroll = app.tool_detail_scroll.saturating_add(1);
                    } else if app.active_modal == Modal::Permission {
                        app.permission_scroll = app
                            .permission_scroll
                            .saturating_add(4)
                            .min(app.permission_max_scroll);
                    } else {
                        app.pin_header_line = None;
                        app.scroll = app.scroll.saturating_add(4).min(app.max_scroll);
                        if app.scroll >= app.max_scroll {
                            app.follow_bottom = true;
                        }
                    }
                }
                input::InputAction::ScrollPageUp => {
                    if app.active_modal == Modal::Permission {
                        let step = app.view_height.saturating_sub(1).max(1) as usize;
                        app.permission_scroll = app.permission_scroll.saturating_sub(step);
                    } else {
                        app.follow_bottom = false;
                        app.pin_header_line = None;
                        // Leave one line of overlap so the reader keeps context.
                        let step = app.view_height.saturating_sub(1).max(1);
                        app.scroll = app.scroll.saturating_sub(step);
                    }
                }
                input::InputAction::ScrollPageDown => {
                    if app.active_modal == Modal::Permission {
                        let step = app.view_height.saturating_sub(1).max(1) as usize;
                        app.permission_scroll = app
                            .permission_scroll
                            .saturating_add(step)
                            .min(app.permission_max_scroll);
                    } else {
                        app.pin_header_line = None;
                        let step = app.view_height.saturating_sub(1).max(1);
                        app.scroll = app.scroll.saturating_add(step).min(app.max_scroll);
                        if app.scroll >= app.max_scroll {
                            app.follow_bottom = true;
                        }
                    }
                }
                input::InputAction::ScrollTop => {
                    if app.active_modal == Modal::Permission {
                        app.permission_scroll = 0;
                    } else {
                        app.follow_bottom = false;
                        app.pin_header_line = None;
                        app.scroll = 0;
                    }
                }
                input::InputAction::ScrollBottom => {
                    if app.active_modal == Modal::Permission {
                        app.permission_scroll = app.permission_max_scroll;
                    } else {
                        app.pin_header_line = None;
                        app.scroll = app.max_scroll;
                        app.follow_bottom = true;
                    }
                }
                input::InputAction::CopySelection => {
                    if let Some(text) = extract_selection_text(
                        &app.selection,
                        app.focused_messages(),
                        &app.input,
                        &app.layout_map,
                    ) {
                        spawn_clipboard_copy(&copy_tx, copy_pending.clone(), text);
                    }
                }
                input::InputAction::CtrlC => {
                    if let Some(text) = extract_selection_text(
                        &app.selection,
                        app.focused_messages(),
                        &app.input,
                        &app.layout_map,
                    ) {
                        spawn_clipboard_copy(&copy_tx, copy_pending.clone(), text);
                    } else if matches!(
                        app.active_modal,
                        Modal::ApiKey | Modal::Endpoint | Modal::ModelName
                    ) {
                        app.input = std::mem::take(&mut app.stashed_input);
                        app.cursor_position = app.input.chars().count();
                        app.setup_solution = None;
                        app.setup_endpoint = None;
                        app.setup_model = None;
                        app.active_modal = Modal::None;
                    } else if app.active_modal == Modal::HistorySearch {
                        // Cancel the fuzzy query: restore the in-progress draft
                        // the user was composing before Ctrl+R.
                        app.input = std::mem::take(&mut app.stashed_input);
                        app.cursor_position = app.input.chars().count();
                        app.input_scroll = 0;
                        app.suggestion_index = None;
                        app.modal_index = 0;
                        app.active_modal = Modal::None;
                    } else if app.active_modal != Modal::None
                        && app.active_modal != Modal::Permission
                    {
                        app.active_modal = Modal::None;
                    } else if runtime.is_responding.load(Ordering::SeqCst) {
                        let _ = app.tx.send(AgentRequest::Interrupt);
                    } else if !app.input.is_empty() {
                        app.input.clear();
                        app.cursor_position = 0;
                        app.input_scroll = 0;
                        app.suggestion_index = None;
                    } else if app.ctrl_c_armed_ticks > 0 {
                        return Ok(());
                    } else {
                        // Arm a ~2s window in which a second Ctrl+C quits.
                        app.ctrl_c_armed_ticks = 20;
                    }
                }
                input::InputAction::ToggleToolSteps => {
                    // Read the target state from the focused view (a snapshot
                    // clone), then apply to the live messages.
                    let expand = app.focused_messages().iter().any(|message| {
                        !message.is_subagent_task() && message.tool_step_expanded() == Some(false)
                    });
                    let mut messages = runtime.messages.lock().await;
                    for message in focused_messages_mut(&mut messages, &app.focus_stack) {
                        // Sub-agent task steps are navigated, not expanded.
                        if !message.is_subagent_task() {
                            message.set_tool_step_expanded(expand);
                        }
                    }
                    drop(messages);
                    // Persist the choice as the global density so new tool steps
                    // created mid-turn also respect it (ADR-0001 Step 8).
                    app.tool_density.store(expand, Ordering::SeqCst);
                    app.selection = SelectionState::None;
                }
                input::InputAction::FocusNextTarget => {
                    app.focus_interactive_target(1);
                }
                input::InputAction::FocusPrevTarget => {
                    app.focus_interactive_target(-1);
                }
                input::InputAction::EnterBrowseZone { backward } => {
                    // Hand keyboard focus from the input box over to the
                    // conversation stream. Direction picks the closest step:
                    // forward (Tab) selects the first one, backward (Shift+Tab)
                    // selects the last one.
                    app.focus_zone = input::FocusZone::Browse;
                    let dir: i8 = if backward { -1 } else { 1 };
                    app.focus_interactive_target(dir);
                }
                input::InputAction::ReturnToComposeZone => {
                    app.focus_zone = input::FocusZone::Compose;
                }
                input::InputAction::ActivateFocusedTarget => {
                    if let Some(target) = app.focused_target {
                        match target.kind {
                            InteractiveTargetKind::ToolStep => {
                                let mut messages = runtime.messages.lock().await;
                                let enter_id = resolve_focused_mut(
                                    &mut messages,
                                    &app.focus_stack,
                                    target.message_idx,
                                )
                                .and_then(|message| {
                                    if message.is_subagent_task() {
                                        message.tool_step_call_id().map(String::from)
                                    } else {
                                        None
                                    }
                                });
                                if let Some(id) = enter_id {
                                    drop(messages);
                                    app.enter_subagent(id);
                                } else {
                                    // Open the full-output detail overlay instead
                                    // of the inline expand/collapse (the latter is
                                    // the cramped UX the redesign replaces). The
                                    // bulk `ctrl+t` toggle still inline-expands
                                    // every step if desired.
                                    drop(messages);
                                    app.tool_detail_message_idx = Some(target.message_idx);
                                    app.tool_detail_scroll = 0;
                                    app.active_modal = Modal::ToolStepDetail;
                                }
                            }
                            InteractiveTargetKind::Thinking => {
                                let mut messages = runtime.messages.lock().await;
                                let toggled =
                                    app.toggle_step_pinned(&mut messages, target.message_idx);
                                drop(messages);
                                if toggled {
                                    app.selection = SelectionState::None;
                                }
                            }
                        }
                    }
                }
                input::InputAction::Paste => {
                    // Ctrl+V: read the system clipboard off the event loop.
                    // The result is delivered back through `paste_rx` and
                    // applied on a later frame (image -> attach, text -> insert).
                    if app.active_modal == Modal::None {
                        spawn_clipboard_paste(&paste_tx);
                    }
                }
                input::InputAction::ExitSubAgent => {
                    app.exit_subagent();
                }
                input::InputAction::PrevSibling => {
                    app.cycle_sibling(-1);
                }
                input::InputAction::NextSibling => {
                    app.cycle_sibling(1);
                }
                input::InputAction::InsertChar(c) => {
                    // Already handled by process_event mutating app.input
                    let _ = c;
                    app.suggestion_index = None;
                }
                input::InputAction::Backspace => {
                    app.suggestion_index = None;
                }
                input::InputAction::CursorLeft => {}
                input::InputAction::CursorRight => {}
                input::InputAction::SuggestNext => {
                    let count = app.completions().len();
                    if count > 0 {
                        let next = match app.suggestion_index {
                            Some(i) => (i + 1) % count,
                            None => 0,
                        };
                        app.suggestion_index = Some(next);
                    }
                }
                input::InputAction::SuggestPrev => {
                    let count = app.completions().len();
                    if count > 0 {
                        let prev = match app.suggestion_index {
                            Some(i) => {
                                if i == 0 {
                                    count - 1
                                } else {
                                    i - 1
                                }
                            }
                            None => count - 1,
                        };
                        app.suggestion_index = Some(prev);
                    }
                }
                input::InputAction::AcceptSuggestion(idx_str) => {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        let completions = app.completions();
                        if let Some(comp) = completions.get(idx) {
                            let replace_start = comp.replace_start;
                            let replace_end = comp.replace_end;
                            let mut label = comp.label.clone();
                            // File accept: append a trailing space so the user
                            // can keep typing their message (matches
                            // opencode's splice behaviour). Directories end in
                            // `/` and the popup re-triggers showing the dir's
                            // contents, so no space is appended there.
                            let is_dir = label.ends_with('/');
                            if !is_dir {
                                let needs_space = app
                                    .input
                                    .get(replace_end..)
                                    .and_then(|s| s.chars().next())
                                    .map(|c| !c.is_whitespace())
                                    .unwrap_or(true);
                                if needs_space {
                                    label.push(' ');
                                }
                            }
                            // Splice `label` into the input over the
                            // `[replace_start, replace_end)` byte range, then
                            // land the cursor just past the inserted text.
                            let mut new_input =
                                String::with_capacity(app.input.len() + label.len());
                            new_input.push_str(&app.input[..replace_start]);
                            new_input.push_str(&label);
                            let cursor_byte = replace_start + label.len();
                            new_input.push_str(&app.input[replace_end..]);
                            app.input = new_input;
                            app.cursor_position = app.input[..cursor_byte].chars().count();
                            // Drop the cached project scan so newly-created
                            // files become visible on the next `@` mention
                            // without a restart.
                            app.path_scan_cache = None;
                        }
                    }
                }
                input::InputAction::HistoryPrev => {
                    if !app.input_history.is_empty() {
                        let new_idx = match app.history_index {
                            Some(i) => {
                                if i == 0 {
                                    0
                                } else {
                                    i - 1
                                }
                            }
                            None => app.input_history.len() - 1,
                        };
                        app.history_index = Some(new_idx);
                        app.input = app.input_history[new_idx].clone();
                        app.cursor_position = app.input.chars().count();
                    }
                }
                input::InputAction::HistoryNext => {
                    if let Some(i) = app.history_index {
                        if i + 1 < app.input_history.len() {
                            let new_idx = i + 1;
                            app.history_index = Some(new_idx);
                            app.input = app.input_history[new_idx].clone();
                            app.cursor_position = app.input.chars().count();
                        } else {
                            app.history_index = None;
                            app.input = String::new();
                            app.cursor_position = 0;
                        }
                    }
                }
                input::InputAction::ModalUp => match app.active_modal {
                    Modal::Models => {
                        app.modal_index = if app.modal_index == 0 {
                            SOLUTIONS.len() - 1
                        } else {
                            app.modal_index - 1
                        };
                    }
                    Modal::HistorySearch => {
                        // Up/Down walk the fuzzy-filtered list, not the raw
                        // history, so the cursor never lands on an entry the
                        // user cannot actually see or select.
                        let count = app.history_filtered().len();
                        app.modal_index = if count == 0 {
                            0
                        } else if app.modal_index == 0 {
                            count - 1
                        } else {
                            app.modal_index - 1
                        };
                    }
                    Modal::Permission => {
                        let count = if app.permission_confirm_always { 2 } else { 3 };
                        app.modal_index = if app.modal_index == 0 {
                            count - 1
                        } else {
                            app.modal_index - 1
                        };
                    }
                    Modal::Sessions => {
                        let count = app.sessions_overview.len();
                        app.modal_index = if count == 0 {
                            0
                        } else if app.modal_index == 0 {
                            count - 1
                        } else {
                            app.modal_index - 1
                        };
                    }
                    Modal::ApiKey
                    | Modal::Endpoint
                    | Modal::ModelName
                    | Modal::Help
                    | Modal::ToolStepDetail
                    | Modal::None => {}
                },
                input::InputAction::ModalDown => match app.active_modal {
                    Modal::Models => {
                        app.modal_index = (app.modal_index + 1) % SOLUTIONS.len();
                    }
                    Modal::HistorySearch => {
                        let count = app.history_filtered().len().max(1);
                        app.modal_index = (app.modal_index + 1) % count;
                    }
                    Modal::Permission => {
                        let count = if app.permission_confirm_always { 2 } else { 3 };
                        app.modal_index = (app.modal_index + 1) % count;
                    }
                    Modal::Sessions => {
                        let count = app.sessions_overview.len().max(1);
                        app.modal_index = (app.modal_index + 1) % count;
                    }
                    Modal::ApiKey
                    | Modal::Endpoint
                    | Modal::ModelName
                    | Modal::Help
                    | Modal::ToolStepDetail
                    | Modal::None => {}
                },
                input::InputAction::PermissionSubmit => {
                    if app.permission_confirm_always {
                        if app.modal_index == 1 {
                            app.permission_confirm_always = false;
                            app.modal_index = 1;
                            break 'event_batch;
                        }
                    } else if app.modal_index == 1 {
                        app.permission_confirm_always = true;
                        app.modal_index = 0;
                        break 'event_batch;
                    }
                    if let Some(request) = app.pending_permission.take() {
                        let decision = if app.permission_confirm_always {
                            PermissionDecision::Always
                        } else {
                            match app.modal_index {
                                0 => PermissionDecision::Once,
                                _ => PermissionDecision::Reject,
                            }
                        };
                        *runtime.pending_permission.lock().await = None;
                        app.active_modal = Modal::None;
                        app.modal_index = 0;
                        app.permission_confirm_always = false;
                        let _ = app.tx.send(AgentRequest::PermissionReply {
                            request_id: request.id,
                            decision,
                        });
                    }
                }
                input::InputAction::PermissionReject => {
                    if let Some(request) = app.pending_permission.take() {
                        *runtime.pending_permission.lock().await = None;
                        app.active_modal = Modal::None;
                        app.modal_index = 0;
                        app.permission_confirm_always = false;
                        let _ = app.tx.send(AgentRequest::PermissionReply {
                            request_id: request.id,
                            decision: PermissionDecision::Reject,
                        });
                    }
                }
                input::InputAction::PermissionBack => {
                    app.permission_confirm_always = false;
                    app.modal_index = 1;
                }
                input::InputAction::SelectionStart { x, y } => {
                    // Hint bar's goal segment: surface the full goal via the
                    // existing `/goal status` command. Acts as the
                    // click-to-expand affordance promised by the hint bar.
                    if app.hint_goal_rect.is_some_and(|r| {
                        r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height
                    }) {
                        let cmd = "/goal status".to_string();
                        runtime.is_responding.store(true, Ordering::SeqCst);
                        *runtime.activity_status.lock().await = "queued".to_string();
                        app.follow_bottom = true;
                        app.pin_header_line = None;
                        runtime
                            .messages
                            .lock()
                            .await
                            .push(TranscriptMessage::new(Role::User, cmd.clone()));
                        let _ = app.tx.send(AgentRequest::SlashCommand(cmd));
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    } else if app.sticky_rect.is_some_and(|r| {
                        // Sticky pinned step header: collapse it on click.
                        r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height
                    }) {
                        if let Some(mi) = app.sticky_step {
                            let mut messages = runtime.messages.lock().await;
                            app.focused_target =
                                app.focused_messages().get(mi).and_then(|message| {
                                    if message.is_thinking() {
                                        Some(InteractiveTarget::thinking(mi))
                                    } else if message.is_tool_step() || message.is_subagent_task() {
                                        Some(InteractiveTarget::tool_step(mi))
                                    } else {
                                        None
                                    }
                                });
                            app.toggle_step_pinned(&mut messages, mi);
                            drop(messages);
                        }
                        // Activating a step via click implies keyboard focus
                        // follows it as well.
                        app.focus_zone = input::FocusZone::Browse;
                        app.selection = SelectionState::None;
                        app.drag.cancel();
                    } else if let Some(cursor) = input::resolve_cursor(&app.layout_map, x, y) {
                        if cursor.message_idx == crate::render::INPUT_MSG_IDX {
                            // Click inside the live input box: hand keyboard
                            // focus back to the prompt so the next keypress
                            // edits rather than navigating steps.
                            app.focus_zone = input::FocusZone::Compose;
                            app.focused_target = None;
                            app.selection = SelectionState::start_range(cursor);
                            app.drag.start(cursor);
                        } else if cursor.block_idx == TOOL_STEP_BLOCK_IDX {
                            // Clicked a tool-step header: navigate into a
                            // sub-agent task, otherwise toggle that step.
                            let mi = cursor.message_idx;
                            app.focused_target = Some(InteractiveTarget::tool_step(mi));
                            let mut messages = runtime.messages.lock().await;
                            let enter_id = resolve_focused_mut(&mut messages, &app.focus_stack, mi)
                                .and_then(|message| {
                                    if message.is_subagent_task() {
                                        message.tool_step_call_id().map(String::from)
                                    } else {
                                        None
                                    }
                                });
                            if let Some(id) = enter_id {
                                drop(messages);
                                app.enter_subagent(id);
                            } else {
                                app.toggle_step_pinned(&mut messages, mi);
                                drop(messages);
                            }
                            app.focus_zone = input::FocusZone::Browse;
                            app.selection = SelectionState::None;
                            app.drag.cancel();
                        } else if cursor.block_idx == THINKING_BLOCK_IDX {
                            // Clicked a reasoning trace header: toggle that trace.
                            let mi = cursor.message_idx;
                            app.focused_target = Some(InteractiveTarget::thinking(mi));
                            let mut messages = runtime.messages.lock().await;
                            app.toggle_step_pinned(&mut messages, mi);
                            drop(messages);
                            app.focus_zone = input::FocusZone::Browse;
                            app.selection = SelectionState::None;
                            app.drag.cancel();
                        } else {
                            // Inside a table cell, a press places the cursor
                            // and starts a drag confined to that cell: the
                            // selection can roam across the cell's wrapped
                            // lines but never crosses `│` borders. A plain
                            // click (no drag) leaves nothing selected.
                            if let Some((mi, bi, cell)) = app.layout_map.table_cell_at(x, y) {
                                app.selection = SelectionState::start_range(cursor);
                                app.drag.start_in_cell(cursor, (mi, bi, cell));
                            } else {
                                app.selection = SelectionState::start_range(cursor);
                                app.drag.start(cursor);
                            }
                            app.focused_target = None;
                            // Clicking anywhere in the conversation content
                            // hands keyboard focus to the stream (Browse), so
                            // the click location always determines the zone.
                            app.focus_zone = input::FocusZone::Browse;
                        }
                    } else {
                        app.selection = SelectionState::None;
                        app.focused_target = None;
                        app.drag.cancel();
                    }
                }
                input::InputAction::SelectionUpdate { x, y } => {
                    // Keep a cell-confined drag from leaking past `│` borders.
                    let (x, y) = if let Some(cell) = app.drag.cell_constraint {
                        app.layout_map
                            .clamp_to_table_cell(cell, x, y)
                            .unwrap_or((x, y))
                    } else {
                        (x, y)
                    };
                    if let Some(cursor) = input::resolve_cursor(&app.layout_map, x, y) {
                        app.selection.update_head(cursor);
                    }
                }
                input::InputAction::SelectionEnd => {
                    app.drag.end();
                    // If selection is empty, clear it
                    if let Some((a, b)) = app.selection.normalized_range() {
                        if a == b {
                            app.selection = SelectionState::None;
                        }
                    }
                }
                input::InputAction::SelectBlock { x, y } => {
                    if let Some((mi, bi)) = input::resolve_block(&app.layout_map, x, y) {
                        app.selection = SelectionState::Block {
                            message_idx: mi,
                            block_idx: bi,
                        };
                    }
                }
                input::InputAction::Hover { x, y } => {
                    // Only reasoning-trace headers carry a hover affordance
                    // today. When the pointer rests on one — either the inline
                    // header (block_idx == THINKING_BLOCK_IDX) or the sticky pinned
                    // variant — record its message index so the next draw
                    // brightens it; otherwise clear it.
                    if app.sticky_rect.is_some_and(|r| {
                        r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height
                    }) {
                        if let Some(mi) = app.sticky_step {
                            let is_thinking = runtime
                                .messages
                                .lock()
                                .await
                                .get(mi)
                                .map(|m| m.is_thinking())
                                .unwrap_or(false);
                            app.hovered_reasoning = is_thinking.then_some(mi);
                        }
                    } else if let Some(cursor) = input::resolve_cursor(&app.layout_map, x, y) {
                        app.hovered_reasoning =
                            (cursor.block_idx == THINKING_BLOCK_IDX).then_some(cursor.message_idx);
                    } else {
                        app.hovered_reasoning = None;
                    }
                }
                input::InputAction::ConfigureKey => {
                    if app.active_modal == Modal::Models {
                        app.setup_solution = Some(app.modal_index);
                        app.setup_endpoint = None;
                        app.setup_model = None;
                        app.stashed_input = std::mem::take(&mut app.input);
                        app.cursor_position = 0;
                        app.active_modal = if SOLUTIONS[app.modal_index].custom_endpoint {
                            Modal::Endpoint
                        } else {
                            Modal::ApiKey
                        };
                    }
                }
                input::InputAction::SubmitEndpoint => {
                    let endpoint = std::mem::take(&mut app.input);
                    if !endpoint.trim().is_empty() {
                        app.setup_endpoint = Some(endpoint.trim().to_string());
                        app.cursor_position = 0;
                        app.active_modal = Modal::ModelName;
                    }
                }
                input::InputAction::SubmitModelName => {
                    let model = std::mem::take(&mut app.input);
                    if !model.trim().is_empty() {
                        app.setup_model = Some(model.trim().to_string());
                        app.cursor_position = 0;
                        app.active_modal = Modal::ApiKey;
                    }
                }
                input::InputAction::SubmitApiKey => {
                    if let Some(idx) = app.setup_solution.take() {
                        let key = std::mem::take(&mut app.input);
                        app.input = std::mem::take(&mut app.stashed_input);
                        app.cursor_position = app.input.chars().count();
                        app.active_modal = Modal::None;
                        if !key.trim().is_empty() {
                            let solution = SOLUTIONS[idx];
                            let _ = app.tx.send(AgentRequest::SwitchProvider {
                                provider_type: solution.id.to_string(),
                                model: app
                                    .setup_model
                                    .take()
                                    .unwrap_or_else(|| solution.model.to_string()),
                                api_key: Some(key.trim().to_string()),
                                base_url: app.setup_endpoint.take(),
                            });
                        }
                    } else {
                        app.active_modal = Modal::None;
                    }
                }
            }
        }
    }
}

fn tool_activity_status(name: &str) -> &'static str {
    match name {
        "read_file" | "list_dir" | "use_skill" => "exploring",
        "grep" => "searching codebase",
        "write_file" | "edit_file" => "making edits",
        "bash" => "running command",
        "goal_checklist" => "updating tasks",
        name if name.starts_with("mcp__") => "using MCP",
        _ => "using tool",
    }
}

/// Snapshot the currently active provider id and model so a freshly created
/// message can be attributed to the model that produced it. The listener keeps
/// these in sync with the harness via `ProviderSwitched` and the initial
/// selection, so live messages stay traceable just like restored ones.
async fn attribution(
    provider: &Arc<Mutex<String>>,
    model: &Arc<Mutex<String>>,
) -> (String, String) {
    (provider.lock().await.clone(), model.lock().await.clone())
}

fn compact_retry_reason(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or(message).trim();
    let mut chars = first_line.chars();
    let prefix = chars.by_ref().take(56).collect::<String>();
    if chars.next().is_some() {
        format!("{}...", prefix)
    } else {
        prefix
    }
}

/// Resolve a mutable reference to the message at index `mi` within the
/// currently focused view: the root conversation when the focus stack is empty,
/// or the focused sub-agent task's child stream otherwise. Selection and layout
/// indices are recorded against whichever slice was rendered, so mutations must
/// resolve through the same context.
fn resolve_focused_mut<'a>(
    messages: &'a mut [TranscriptMessage],
    focus_stack: &[String],
    mi: usize,
) -> Option<&'a mut TranscriptMessage> {
    let Some(current) = focus_stack.last() else {
        return messages.get_mut(mi);
    };
    let task_idx = messages.iter().position(|message| {
        message.is_subagent_task() && message.tool_step_call_id() == Some(current.as_str())
    })?;
    messages[task_idx].subagent_children_mut()?.get_mut(mi)
}

/// Iterate mutable messages in the currently focused view (the root
/// conversation, or the focused sub-agent task's child stream) for bulk
/// expand/collapse operations. Callers filter by kind as needed.
fn focused_messages_mut<'a>(
    messages: &'a mut [TranscriptMessage],
    focus_stack: &[String],
) -> Box<dyn Iterator<Item = &'a mut TranscriptMessage> + 'a> {
    match focus_stack.last() {
        None => Box::new(messages.iter_mut()),
        Some(current) => {
            let task_idx = messages.iter().position(|message| {
                message.is_subagent_task() && message.tool_step_call_id() == Some(current.as_str())
            });
            match task_idx {
                Some(idx) => match messages[idx].subagent_children_mut() {
                    Some(children) => Box::new(children.iter_mut()),
                    None => Box::new(std::iter::empty()),
                },
                None => Box::new(std::iter::empty()),
            }
        }
    }
}

/// Extract selected text from either transcript messages or the live input box,
/// depending on which the semantic selection covers.
fn extract_selection_text(
    sel: &SelectionState,
    messages: &[crate::document::TranscriptMessage],
    input: &str,
    layout_map: &crate::layout::LayoutMap,
) -> Option<String> {
    let on_input = match sel {
        SelectionState::None => false,
        SelectionState::Block { message_idx, .. } => *message_idx == crate::render::INPUT_MSG_IDX,
        SelectionState::TableCell { message_idx, .. } => {
            *message_idx == crate::render::INPUT_MSG_IDX
        }
        SelectionState::Range { anchor, head } => {
            anchor.message_idx == crate::render::INPUT_MSG_IDX
                && head.message_idx == crate::render::INPUT_MSG_IDX
        }
    };
    if !on_input {
        return get_selected_text(sel, messages, &|mi, bi| layout_map.table_grid(mi, bi));
    }
    match sel {
        SelectionState::Block { .. } => Some(input.to_string()),
        SelectionState::Range { anchor, head } => {
            let (start, end) = if anchor.byte_offset <= head.byte_offset {
                (anchor.byte_offset, head.byte_offset)
            } else {
                (head.byte_offset, anchor.byte_offset)
            };
            let start = floor_char_boundary(input, start);
            let end = inclusive_end(input, end);
            (start < end).then(|| input[start..end].to_string())
        }
        _ => None,
    }
}

fn spawn_clipboard_copy(
    tx: &mpsc::UnboundedSender<Result<clipboard::CopyOutcome, String>>,
    copy_pending: Arc<AtomicUsize>,
    text: String,
) {
    let tx = tx.clone();
    copy_pending.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            crate::clipboard::copy(&text),
        )
        .await
        {
            Ok(inner) => inner,
            Err(_) => Err("clipboard copy timed out".to_string()),
        };
        let _ = tx.send(result);
        copy_pending.fetch_sub(1, Ordering::SeqCst);
    });
}

/// Read the system clipboard in a background task and deliver the result to
/// the event loop. Bounded by a timeout so a stuck clipboard reader can never
/// freeze paste feedback.
fn spawn_clipboard_paste(tx: &mpsc::UnboundedSender<clipboard::ClipboardRead>) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let read =
            match tokio::time::timeout(std::time::Duration::from_secs(3), crate::clipboard::read())
                .await
            {
                Ok(inner) => inner,
                Err(_) => clipboard::ClipboardRead::Empty,
            };
        let _ = tx.send(read);
    });
}

/// Apply a completed clipboard paste: attach an image, insert text at the
/// cursor, or surface an error toast.
fn apply_clipboard_paste(app: &mut App, read: clipboard::ClipboardRead) {
    if app.active_modal != Modal::None {
        return;
    }
    match read {
        clipboard::ClipboardRead::Image { data, mime } => {
            let encoded = crate::clipboard::base64_image(&data);
            app.pending_images.push(ImagePart {
                mime,
                data: encoded,
            });
            let n = app.pending_images.len();
            app.copy_toast_message = format!(
                "{n} image{} attached — enter to send",
                if n == 1 { "" } else { "s" }
            );
            app.copy_toast_failed = false;
            app.copy_toast_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(1800));
        }
        clipboard::ClipboardRead::Text(text) => {
            let chars_to_insert = text.chars().count();
            let byte_pos = app
                .input
                .char_indices()
                .map(|(i, _)| i)
                .nth(app.cursor_position)
                .unwrap_or(app.input.len());
            app.input.insert_str(byte_pos, &text);
            app.cursor_position += chars_to_insert;
            app.copy_toast_message = format!(
                "pasted {chars_to_insert} char{}",
                if chars_to_insert == 1 { "" } else { "s" }
            );
            app.copy_toast_failed = false;
            app.copy_toast_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(1200));
        }
        clipboard::ClipboardRead::Empty => {
            app.copy_toast_message = "clipboard is empty".to_string();
            app.copy_toast_failed = true;
            app.copy_toast_until =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(1200));
        }
    }
}

fn set_copy_feedback(app: &mut App, result: Result<clipboard::CopyOutcome, String>) {
    match result {
        Ok(clipboard::CopyOutcome::Native) => {
            app.copy_toast_message = "copied to clipboard".to_string();
            app.copy_toast_failed = false;
        }
        Ok(clipboard::CopyOutcome::Osc52) => {
            app.copy_toast_message = "copy sent via OSC52".to_string();
            app.copy_toast_failed = false;
        }
        Err(error) => {
            let mut chars = error.chars();
            let prefix = chars.by_ref().take(48).collect::<String>();
            app.copy_toast_message = if chars.next().is_some() {
                format!("copy failed: {}...", prefix)
            } else {
                format!("copy failed: {}", prefix)
            };
            app.copy_toast_failed = true;
        }
    }
}

fn display_status(loop_status: &str, activity: &str, awaiting_permission: bool) -> String {
    let activity = if awaiting_permission {
        "awaiting permission"
    } else {
        activity
    };
    match (loop_status, activity) {
        ("idle", "") => "idle".to_string(),
        ("idle", activity) => activity.to_string(),
        (loop_status, "") => loop_status.to_string(),
        (loop_status, activity) => format!("{} · {}", loop_status, activity),
    }
}

pub async fn start_tui(
    tx: mpsc::UnboundedSender<AgentRequest>,
    rx: mpsc::UnboundedReceiver<AgentResponse>,
    initial_provider: String,
    initial_model: String,
    input_history: Vec<String>,
    initial_messages: Vec<Message>,
    custom_commands: Vec<(String, String)>,
    mcp_statuses: Vec<(String, McpConnectionStatus)>,
) -> Result<Vec<String>, Box<dyn Error>> {
    run_tui(
        tx,
        rx,
        initial_provider,
        initial_model,
        input_history,
        initial_messages,
        custom_commands,
        mcp_statuses,
    )
    .await
}

fn transcript_message_from_core(message: Message) -> Option<TranscriptMessage> {
    if message.hidden || message.role == Role::System {
        return None;
    }
    let provider = message.provider.clone();
    let model = message.model.clone();
    let content = if let Some(display_content) = message.display_content {
        display_content
    } else if message.content.is_empty() {
        message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|call| format_tool_call(&call.name, &call.arguments))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        message.content
    };
    if content.is_empty() {
        None
    } else {
        let mut msg = TranscriptMessage::new(message.role, content);
        msg.provider = provider;
        msg.model = model;
        Some(msg)
    }
}

fn transcript_messages_from_core(messages: Vec<Message>) -> Vec<TranscriptMessage> {
    let mut restored = Vec::new();
    for mut message in messages {
        if message.hidden || message.role == Role::System {
            continue;
        }
        // Attribution travels on every part so a resumed session that mixed
        // models still shows which model produced each turn.
        let provider = message.provider.clone();
        let model = message.model.clone();
        if message.role == Role::Assistant {
            if let Some(reasoning) = message.reasoning_content.take() {
                let mut thinking = TranscriptMessage::thinking(reasoning);
                thinking.provider = provider.clone();
                thinking.model = model.clone();
                thinking.set_thinking_duration(0);
                restored.push(thinking);
            }
            if let Some(calls) = message.tool_calls.take() {
                for call in calls {
                    // Historical results match by tool name, so use it as the id.
                    let mut step =
                        TranscriptMessage::tool_step(call.name.clone(), call.name, call.arguments);
                    step.provider = provider.clone();
                    step.model = model.clone();
                    restored.push(step);
                }
                if message.content.is_empty() {
                    continue;
                }
            }
        }
        if message.role == Role::Tool {
            if let Some((name, output)) = parse_tool_result(&message.content) {
                if restored
                    .iter_mut()
                    .any(|item| {
                        item.finish_tool_step(
                            name,
                            output,
                            neenee_core::ToolOutput::text(output),
                            0,
                        )
                    })
                {
                    continue;
                }
            }
        }
        if let Some(message) = transcript_message_from_core(message) {
            restored.push(message);
        }
    }
    restored
}

fn parse_tool_result(content: &str) -> Option<(&str, &str)> {
    let content = content.strip_prefix('[')?;
    let (name, output) = content.split_once(" result]:")?;
    Some((name, output.trim_start_matches('\n')))
}

fn format_tool_call(name: &str, arguments: &str) -> String {
    let arguments = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| arguments.to_string());
    format!("Calling `{}`\n\n```json\n{}\n```", name, arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use neenee_core::ToolCall;

    #[test]
    fn restored_history_hides_harness_messages() {
        assert!(transcript_message_from_core(Message::hidden(Role::User, "internal")).is_none());
        assert!(transcript_message_from_core(Message::new(Role::System, "system")).is_none());
    }
    #[test]
    fn restored_history_uses_command_display_content() {
        let message = Message::new(Role::User, "Expanded internal prompt")
            .with_display_content("/review working-tree");
        let restored = transcript_message_from_core(message).unwrap();
        assert_eq!(restored.raw, "/review working-tree");
    }

    #[test]
    fn restored_assistant_carries_provider_and_model_attribution() {
        // A persisted assistant message stamped by the harness keeps its
        // provider/model so a resumed session that mixed models stays
        // traceable in the transcript.
        let message = Message::new(Role::Assistant, "Hello from kimi")
            .with_attribution("kimi-code", "kimi-for-coding");
        let restored = transcript_message_from_core(message).unwrap();
        assert_eq!(restored.provider.as_deref(), Some("kimi-code"));
        assert_eq!(restored.model.as_deref(), Some("kimi-for-coding"));
        assert_eq!(
            restored.attribution_label(),
            Some(("kimi-code".to_string(), "kimi-for-coding".to_string()))
        );

        // A plain user message carries no attribution.
        let user = transcript_message_from_core(Message::new(Role::User, "hi")).unwrap();
        assert!(user.attribution_label().is_none());

        // A provider without an id still surfaces the model alone.
        let model_only = Message::new(Role::Assistant, "x").with_attribution("", "gpt-4o");
        let restored = transcript_message_from_core(model_only).unwrap();
        assert_eq!(
            restored.attribution_label(),
            Some((String::new(), "gpt-4o".to_string()))
        );
    }

    #[test]
    fn restored_reasoning_is_not_shown_as_running() {
        let message = Message {
            role: Role::Assistant,
            content: String::new(),
            display_content: None,
            reasoning_content: Some("step-by-step reasoning".to_string()),
            tool_calls: None,
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
        };

        let restored = transcript_messages_from_core(vec![message]);
        assert_eq!(restored.len(), 1);
        let thinking = &restored[0];
        assert!(thinking.is_thinking());
        // A finished reasoning block must not be rendered with a live spinner.
        assert!(
            thinking.thinking_header().unwrap().contains("0ms"),
            "restored thinking should have a finished duration, got {:?}",
            thinking.thinking_header()
        );
    }

    #[test]
    fn restored_native_tool_calls_are_visible() {
        let message = Message {
            role: Role::Assistant,
            content: String::new(),
            display_content: None,
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call".to_string(),
                name: "read_file".to_string(),
                arguments: "{\"path\":\"README.md\"}".to_string(),
            }]),
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
        };

        let restored = transcript_message_from_core(message).unwrap();
        assert!(restored.raw.contains("read_file"));
    }

    #[test]
    fn restored_tool_results_merge_into_steps_in_fifo_order() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: String::new(),
                display_content: None,
                reasoning_content: None,
                tool_calls: Some(vec![
                    ToolCall {
                        id: "one".to_string(),
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"one"}"#.to_string(),
                    },
                    ToolCall {
                        id: "two".to_string(),
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"two"}"#.to_string(),
                    },
                ]),
                tool_call_id: None,
                images: None,
                provider: None,
                model: None,
                hidden: false,
            },
            Message::tool_result(
                &ToolCall {
                    id: "one".to_string(),
                    name: "read_file".to_string(),
                    arguments: String::new(),
                },
                "[read_file result]:\nfirst",
            ),
            Message::tool_result(
                &ToolCall {
                    id: "two".to_string(),
                    name: "read_file".to_string(),
                    arguments: String::new(),
                },
                "[read_file result]:\nsecond",
            ),
        ];

        let mut restored = transcript_messages_from_core(messages);
        assert_eq!(restored.len(), 2);
        restored[0].set_tool_step_expanded(true);
        restored[1].set_tool_step_expanded(true);
        assert!(restored[0].raw.contains("first"));
        assert!(!restored[0].raw.contains("second"));
        assert!(restored[1].raw.contains("second"));
    }

    #[test]
    fn tool_activity_is_semantic_and_loop_progress_is_preserved() {
        assert_eq!(tool_activity_status("grep"), "searching codebase");
        assert_eq!(tool_activity_status("edit_file"), "making edits");
        assert_eq!(tool_activity_status("goal_checklist"), "updating tasks");
        assert_eq!(tool_activity_status("mcp__github__search"), "using MCP");
        assert_eq!(
            display_status("loop 2/8", "running command", false),
            "loop 2/8 · running command"
        );
        assert_eq!(
            display_status("loop 2/8", "running command", true),
            "loop 2/8 · awaiting permission"
        );
        assert_eq!(
            compact_retry_reason("rate limited\nfull response body"),
            "rate limited"
        );
    }

    /// Build a small conversation with two sibling sub-agent tasks, each with a
    /// couple of child messages, for focus-navigation tests.
    fn conversation_with_subagents() -> Vec<TranscriptMessage> {
        let mut a = TranscriptMessage::tool_step(
            "task_a",
            "task",
            r#"{"description":"explore a","prompt":"..."}"#,
        );
        a.subagent_children_mut()
            .unwrap()
            .push(TranscriptMessage::new(Role::Assistant, "child A1"));
        let mut b = TranscriptMessage::tool_step(
            "task_b",
            "task",
            r#"{"description":"explore b","prompt":"..."}"#,
        );
        b.subagent_children_mut()
            .unwrap()
            .push(TranscriptMessage::new(Role::Assistant, "child B1"));
        vec![
            TranscriptMessage::new(Role::User, "hi"),
            a,
            TranscriptMessage::new(Role::Assistant, "ok"),
            b,
        ]
    }

    #[test]
    fn resolve_focused_mut_indexes_root_when_unfocused() {
        let mut messages = conversation_with_subagents();
        let focus: Vec<String> = Vec::new();
        let resolved = resolve_focused_mut(&mut messages, &focus, 2);
        assert_eq!(resolved.map(|m| m.raw.clone()).as_deref(), Some("ok"));
    }

    #[test]
    fn resolve_focused_mut_indexes_children_when_focused() {
        let mut messages = conversation_with_subagents();
        let focus = vec!["task_b".to_string()];
        // Index 0 inside task_b's children => "child B1".
        let resolved = resolve_focused_mut(&mut messages, &focus, 0);
        assert_eq!(resolved.map(|m| m.raw.clone()).as_deref(), Some("child B1"));
        // Indexing task_a's children via task_b focus returns none / out of range.
        assert!(resolve_focused_mut(&mut messages, &focus, 5).is_none());
    }

    #[test]
    fn focused_tool_steps_mut_only_touches_focused_subagent_children() {
        let mut messages = conversation_with_subagents();
        // Focused on task_a: its single child is an assistant message (not a
        // tool step), so the focused stream has 1 message and 0 tool steps.
        let focus = vec!["task_a".to_string()];
        let total = focused_messages_mut(&mut messages, &focus).count();
        assert_eq!(total, 1);
        let tool_steps = focused_messages_mut(&mut messages, &focus)
            .filter(|m| m.is_tool_step())
            .count();
        assert_eq!(tool_steps, 0);

        // Root view: 4 messages total, 2 of which are tool steps.
        let focus: Vec<String> = Vec::new();
        assert_eq!(focused_messages_mut(&mut messages, &focus).count(), 4);
        let tool_steps = focused_messages_mut(&mut messages, &focus)
            .filter(|m| m.is_tool_step())
            .count();
        assert_eq!(tool_steps, 2);
    }

    // ----- `@path` completion tests -----

    #[test]
    fn mention_range_detects_at_start_of_input() {
        // Cursor at end of `@src`: range covers the whole token.
        assert_eq!(mention_range_at("@src", 4), Some((0, 4)));
    }

    #[test]
    fn mention_range_detects_inline_after_whitespace() {
        // `look at @src`: the `@` follows a space, so the range starts at the
        // `@` and ends at the cursor.
        assert_eq!(mention_range_at("look at @src", 12), Some((8, 12)));
    }

    #[test]
    fn mention_range_rejects_email_style_at() {
        // `user@host` — the char before `@` is non-whitespace, so no mention.
        assert_eq!(mention_range_at("user@host", 9), None);
    }

    #[test]
    fn mention_range_rejects_whitespace_between_at_and_cursor() {
        // `@src foo`: the cursor sits after a space, walking back crosses
        // whitespace before reaching `@`, so no mention.
        assert_eq!(mention_range_at("@src foo", 8), None);
    }

    #[test]
    fn mention_range_rejects_cursor_before_at() {
        // Cursor before the `@`: nothing to walk back to.
        assert_eq!(mention_range_at("look @src", 4), None);
    }

    #[test]
    fn mention_range_handles_multibyte_before_at() {
        // `中文 @x` — the `@` is preceded by an ASCII space, so we detect it
        // even when multibyte chars appear earlier in the input.
        let s = "中文 @x";
        // Byte offset of the cursor at end (after `x`).
        let cursor_byte = s.len();
        let at_byte = s.find('@').unwrap();
        assert_eq!(mention_range_at(s, cursor_byte), Some((at_byte, cursor_byte)));
    }

    #[test]
    fn path_query_match_empty_query_keeps_top_level_only() {
        // Empty query: only top-level entries survive.
        assert!(path_query_match("Cargo.toml", ""));
        assert!(path_query_match("src/", ""));
        assert!(!path_query_match("src/main.rs", ""));
        assert!(!path_query_match("src/nested/deep.rs", ""));
    }

    #[test]
    fn path_query_match_substring_case_insensitive() {
        // `@cargo` matches `Cargo.toml` regardless of case.
        assert!(path_query_match("Cargo.toml", "cargo"));
        assert!(path_query_match("src/Cargo.toml", "cargo"));
        assert!(!path_query_match("README.md", "cargo"));
    }

    #[test]
    fn path_query_match_directory_descend_on_trailing_slash() {
        // `@src/` is a directory descend: prefix-match to enumerate its
        // descendants, NOT every path containing `src/` anywhere.
        assert!(path_query_match("src/main.rs", "src/"));
        assert!(path_query_match("src/components/button.rs", "src/"));
        assert!(!path_query_match("tests/src_runner.rs", "src/"));
    }

    #[test]
    fn path_query_match_mid_path_substring() {
        // `@src/foo` falls through to plain substring (no trailing slash),
        // so it only matches paths that literally contain `src/foo`.
        assert!(path_query_match("src/foo.rs", "src/foo"));
        assert!(path_query_match("src/foo/bar.rs", "src/foo"));
        // `src/components/foo.rs` does NOT contain `src/foo` as a substring,
        // so it is excluded — the user can type `@foo` instead for a wider
        // filename match.
        assert!(!path_query_match("src/components/foo.rs", "src/foo"));
        assert!(!path_query_match("src/bar.rs", "src/foo"));
    }

    #[test]
    fn history_filtered_ranks_and_filters_input_history() {
        // The App-level view of the Ctrl+R modal: an empty query surfaces
        // every entry unhighlighted, a fuzzy query surfaces only the
        // subsequence matches ordered by score with input order on ties.
        let (mut app, _tmp) = app_in_tempdir(&[], &[]);
        app.input_history = vec![
            "scatter".to_string(),     // idx 0 — 'cat' mid-word, lowest score
            "catalog".to_string(),     // idx 1 — 'cat' at boundary, high score
            "cargo build".to_string(), // idx 2 — 'cat' is not a subsequence
            "the cat sat".to_string(), // idx 3 — 'cat' at boundary, high score
        ];

        // Empty query → all four entries, score 0, no highlight positions.
        app.input.clear();
        let ranked = app.history_filtered();
        assert_eq!(ranked.len(), 4);
        for (_, m) in &ranked {
            assert_eq!(m.score, 0);
            assert!(m.positions.is_empty());
        }

        // Query "cat" → matches catalog, "the cat sat", and scatter; not
        // "cargo build" (no 't' after the 'ca'). Boundary matches outrank
        // scatter, and stable-sort keeps catalog before "the cat sat".
        app.input = "cat".to_string();
        let ranked = app.history_filtered();
        let indices: Vec<usize> = ranked.iter().map(|(i, _)| *i).collect();
        assert_eq!(
            indices,
            vec![1, 3, 0],
            "boundary matches first, then scatter"
        );
        assert!(ranked[0].1.score > ranked[2].1.score);
        // Every matched entry exposes highlight positions, one per query char.
        for (_, m) in &ranked {
            assert_eq!(m.positions.len(), 3);
        }

        // Query with no subsequence match → empty filtered list (the renderer
        // turns this into the "no matches" placeholder).
        app.input = "xyz".to_string();
        assert!(app.history_filtered().is_empty());
    }

    /// Build a minimal `App` scoped to a tempdir project so we can exercise
    /// the completion pipeline end-to-end without touching the user's real
    /// filesystem. Mirrors how a real session captures cwd at startup.
    fn app_in_tempdir(files: &[&str], dirs: &[&str]) -> (App, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        for d in dirs {
            std::fs::create_dir_all(tmp.path().join(d)).expect("mkdir");
        }
        for f in files {
            // Create parent dirs as needed so `src/foo.rs` lays down cleanly.
            let path = tmp.path().join(f);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("mkdir for file");
            }
            std::fs::write(path, "x").expect("write file");
        }
        let cwd = tmp.path().to_path_buf();
        let app = App {
            input: String::new(),
            messages: Vec::new(),
            scroll: 0,
            follow_bottom: true,
            content_lines: 0,
            view_height: 0,
            max_scroll: 0,
            sticky_step: None,
            sticky_rect: None,
            hint_goal_rect: None,
            sticky_header_line: None,
            pin_header_line: None,
            focus_stack: Vec::new(),
            tx: new_test_channel(),
            should_quit: Arc::new(AtomicBool::new(false)),
            suggestion_index: None,
            custom_commands: Vec::new(),
            cursor_position: 0,
            input_scroll: 0,
            active_modal: Modal::None,
            modal_index: 0,
            current_provider: "mock".to_string(),
            current_model: "mock".to_string(),
            cwd_display: ".".to_string(),
            cwd: cwd.clone(),
            path_scan_cache: None,
            current_goal: None,
            loop_status: "idle".to_string(),
            activity_status: String::new(),
            pending_permission: None,
            sessions_overview: Vec::new(),
            permission_confirm_always: false,
            permission_scroll: 0,
            permission_max_scroll: 0,
            input_history: Vec::new(),
            history_index: None,
            pending_images: Vec::new(),
            selection: SelectionState::None,
            drag: SelectionDrag::default(),
            layout_map: LayoutMap::new(),
            hovered_reasoning: None,
            tool_density: Arc::new(AtomicBool::new(false)),
            tool_detail_message_idx: None,
            tool_detail_scroll: 0,
            focused_target: None,
            focus_zone: input::FocusZone::Compose,
            cursor_hidden: false,
            copy_toast_until: None,
            copy_toast_message: String::new(),
            copy_toast_failed: false,
            ctrl_c_armed_ticks: 0,
            esc_armed_ticks: 0,
            spinner_tick: 0,
            stashed_input: String::new(),
            setup_solution: None,
            setup_endpoint: None,
            setup_model: None,
            key_status: HashMap::new(),
            theme: Theme::default(),
            mcp_statuses: Vec::new(),
        };
        (app, tmp)
    }

    /// Stand-up helper for tests that just need a sender half of the agent
    /// channel; the receiver is dropped because no test drives the agent loop.
    fn new_test_channel() -> mpsc::UnboundedSender<AgentRequest> {
        let (tx, _rx) = mpsc::unbounded_channel();
        tx
    }

    #[test]
    fn completions_returns_empty_when_input_does_not_trigger() {
        // Plain text without `@` or `/` produces no completions.
        let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml"], &[]);
        app.input = "hello world".to_string();
        app.cursor_position = app.input.chars().count();
        assert!(app.completions().is_empty());
        assert_eq!(app.completion_kind(), CompletionKind::None);
    }

    #[test]
    fn completions_classifies_slash_input_as_slash_kind() {
        let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml"], &[]);
        app.input = "/go".to_string();
        app.cursor_position = app.input.chars().count();
        let completions = app.completions();
        assert_eq!(app.completion_kind(), CompletionKind::Slash);
        assert!(completions.iter().any(|c| c.label == "/goal"));
        // Slash candidates replace the whole input.
        for c in &completions {
            assert_eq!(c.replace_start, 0);
            assert_eq!(c.replace_end, app.input.len());
        }
    }

    #[test]
    fn completions_path_returns_top_level_for_bare_at() {
        // A bare `@` lists top-level entries only: the file plus the
        // synthesized top-level directory entry.
        let (mut app, _tmp) =
            app_in_tempdir(&["Cargo.toml", "src/main.rs", "README.md"], &["src"]);
        app.input = "@".to_string();
        app.cursor_position = 1;
        let completions = app.completions();
        assert_eq!(app.completion_kind(), CompletionKind::Path);

        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        // Dirs come first alphabetically, then files alphabetically.
        assert!(labels.contains(&"src/"));
        assert!(labels.contains(&"Cargo.toml"));
        assert!(labels.contains(&"README.md"));
        // No nested paths leak into the bare-`@` menu.
        assert!(!labels.iter().any(|l| l.contains("main.rs")));
        // Replace range points just past the `@` (byte 1), ends at cursor (1).
        for c in &completions {
            assert_eq!(c.replace_start, 1);
            assert_eq!(c.replace_end, 1);
            assert!(c.description.is_empty(), "path menu carries no description");
        }
    }

    #[test]
    fn completions_path_descends_into_subdirectory() {
        // `@src/` triggers directory descend: only paths under `src/` match.
        let (mut app, _tmp) = app_in_tempdir(
            &["src/main.rs", "src/util/mod.rs", "tests/smoke.rs"],
            &["src", "src/util", "tests"],
        );
        app.input = "@src/".to_string();
        app.cursor_position = app.input.chars().count();
        let completions = app.completions();
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"src/"));
        assert!(labels.contains(&"src/main.rs"));
        assert!(labels.contains(&"src/util/"));
        assert!(labels.contains(&"src/util/mod.rs"));
        // Nothing from `tests/` leaks in — descend is a prefix match.
        assert!(!labels.iter().any(|l| l.contains("tests")));
    }

    #[test]
    fn completions_path_substring_match_picks_files_across_dirs() {
        // `@main` finds `src/main.rs` via substring match.
        let (mut app, _tmp) = app_in_tempdir(&["src/main.rs", "lib/other.rs"], &["src", "lib"]);
        app.input = "@main".to_string();
        app.cursor_position = app.input.chars().count();
        let completions = app.completions();
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"src/main.rs"));
        assert!(!labels.iter().any(|l| l.contains("other.rs")));
    }

    #[test]
    fn completions_path_skips_dotgit_directory() {
        // `.git/` is always excluded even though hidden files are kept.
        let (mut app, _tmp) = app_in_tempdir(
            &[".git/HEAD", ".git/config", "src/main.rs", ".env"],
            &[".git", "src"],
        );
        app.input = "@".to_string();
        app.cursor_position = 1;
        let completions = app.completions();
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        // Hidden files like `.env` are listed; `.git/` and its contents are not.
        assert!(labels.contains(&".env"));
        assert!(labels.contains(&"src/"));
        assert!(!labels.iter().any(|l| l.starts_with(".git")));
    }

    #[test]
    fn completions_path_cache_populated_once() {
        // The scan should run only the first time `@` triggers; we verify by
        // observing `path_scan_cache` transitioning from None to Some.
        let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml"], &[]);
        assert!(app.path_scan_cache.is_none());
        app.input = "@".to_string();
        app.cursor_position = 1;
        let _ = app.completions();
        let first_scan = app
            .path_scan_cache
            .as_ref()
            .expect("scan populated")
            .clone();
        // A second call must not re-scan: cache stays the same Vec pointer
        // content. We compare lengths because the Vec itself may move.
        app.input = "@Ca".to_string();
        app.cursor_position = app.input.chars().count();
        let _ = app.completions();
        let second_scan = app
            .path_scan_cache
            .as_ref()
            .expect("scan still populated")
            .clone();
        assert_eq!(first_scan.entries, second_scan.entries);
    }

    #[test]
    fn manual_walk_returns_files_and_synthesized_dirs() {
        // The manual fallback path (used when rg is missing) must still
        // produce directory entries with trailing slashes and skip `.git`.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("src/nested")).unwrap();
        std::fs::write(tmp.path().join("src/nested/foo.rs"), "x").unwrap();
        std::fs::write(tmp.path().join("top.md"), "x").unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git/HEAD"), "x").unwrap();

        let entries = manual_walk(tmp.path());
        assert!(entries.contains(&"top.md".to_string()));
        assert!(entries.contains(&"src/".to_string()));
        assert!(entries.contains(&"src/nested/".to_string()));
        assert!(entries.contains(&"src/nested/foo.rs".to_string()));
        assert!(!entries.iter().any(|e| e.starts_with(".git")));
    }
}
