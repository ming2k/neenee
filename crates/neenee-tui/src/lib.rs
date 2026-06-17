pub mod clipboard;
pub mod document;
pub mod input;
pub mod layout;
pub mod render;
pub mod selection;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use neenee_core::{
    AgentMode, AgentRequest, AgentResponse, Goal, HarnessSnapshot, Message, PermissionDecision,
    PermissionRequest, Role, SessionOverview,
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::Rect,
    Terminal,
};
use std::{
    collections::HashMap,
    error::Error,
    io,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
};
use tokio::sync::{mpsc, Mutex};
use unicode_width::UnicodeWidthStr;

use crate::document::{ChatMessage, MessageKind};
use crate::layout::LayoutMap;
use crate::render::Theme;
use crate::selection::{get_selected_text, SelectionDrag, SelectionState};

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/models", "List available LLM backends"),
    ("/mode", "Switch between Build and Plan modes"),
    ("/mcp", "Show MCP server connections and discovered tools"),
    ("/permissions", "Show or clear always-allowed tool rules"),
    ("/session", "Inspect or reset the durable session"),
    ("/sessions", "Browse and resume past sessions"),
    ("/resume", "Resume the most recent cached session"),
    ("/compact", "Compact older complete turns now"),
    ("/goal", "Set or inspect the persistent harness goal"),
    (
        "/loop",
        "Run the active goal for bounded autonomous iterations",
    ),
    ("/init", "Initialize a .neenee/ config tree in this project"),
    ("/exit", "Gracefully exit the program"),
    ("/help", "Show available commands and usage"),
    ("/clear", "Clear the conversation history"),
];

#[derive(Clone, Copy)]
pub(crate) struct ModelSolution {
    pub id: &'static str,
    pub name: &'static str,
    pub model: &'static str,
    pub description: &'static str,
    pub custom_endpoint: bool,
}

const SOLUTIONS: &[ModelSolution] = &[
    ModelSolution {
        id: "kimi-code",
        name: "Kimi Code",
        model: "kimi-for-coding",
        description: "Kimi coding subscription (auto-updated model)",
        custom_endpoint: false,
    },
    ModelSolution {
        id: "openai",
        name: "OpenAI",
        model: "gpt-4o",
        description: "OpenAI API",
        custom_endpoint: false,
    },
    ModelSolution {
        id: "gemini",
        name: "Gemini",
        model: "gemini-1.5-flash",
        description: "Google Gemini",
        custom_endpoint: false,
    },
    ModelSolution {
        id: "kimi",
        name: "Kimi Platform",
        model: "moonshot-v1-8k",
        description: "Moonshot pay-as-you-go API",
        custom_endpoint: false,
    },
    ModelSolution {
        id: "deepseek",
        name: "DeepSeek",
        model: "deepseek-chat",
        description: "DeepSeek AI",
        custom_endpoint: false,
    },
    ModelSolution {
        id: "qwen",
        name: "Qwen",
        model: "qwen-plus",
        description: "Alibaba DashScope",
        custom_endpoint: false,
    },
    ModelSolution {
        id: "glm",
        name: "GLM",
        model: "glm-4-plus",
        description: "Zhipu AI",
        custom_endpoint: false,
    },
    ModelSolution {
        id: "volcengine",
        name: "Volcengine",
        model: "deepseek-v3-250324",
        description: "ByteDance Ark",
        custom_endpoint: false,
    },
    ModelSolution {
        id: "llama",
        name: "Llama",
        model: "local-model",
        description: "Local Llama server",
        custom_endpoint: false,
    },
    ModelSolution {
        id: "custom",
        name: "Custom relay",
        model: "custom-model",
        description: "OpenAI-compatible endpoint",
        custom_endpoint: true,
    },
    ModelSolution {
        id: "mock",
        name: "Mock",
        model: "mock-model",
        description: "Test provider",
        custom_endpoint: false,
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
}

pub struct App {
    pub input: String,
    /// Structured chat messages (semantic document model).
    pub messages: Vec<ChatMessage>,
    pub scroll: u16,
    /// Whether the view follows the newest content (auto-scroll to bottom).
    pub follow_bottom: bool,
    /// Last measured stream height in lines and viewport height, used to pin
    /// the view to the bottom while following.
    pub content_lines: usize,
    pub view_height: u16,
    pub max_scroll: u16,
    /// Expanded card pinned under the HUD bar (its message index + screen rect),
    /// when its body is scrolled into view. Clicks inside the rect collapse it.
    pub sticky_card: Option<usize>,
    pub sticky_rect: Option<ratatui::layout::Rect>,
    pub tx: mpsc::UnboundedSender<AgentRequest>,
    pub should_quit: Arc<AtomicBool>,
    pub suggestion_index: Option<usize>,
    pub custom_commands: Vec<(String, String)>,
    pub cursor_position: usize,
    pub active_modal: Modal,
    pub modal_index: usize,
    pub current_provider: String,
    pub current_model: String,
    pub current_mode: AgentMode,
    pub current_goal: Option<Goal>,
    pub loop_status: String,
    pub activity_status: String,
    pub pending_permission: Option<PermissionRequest>,
    /// Rows shown in the sessions picker (`/sessions` or `neenee resume`).
    pub sessions_overview: Vec<SessionOverview>,
    pub permission_confirm_always: bool,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
    /// Semantic selection state.
    pub selection: SelectionState,
    /// Drag gesture state.
    pub drag: SelectionDrag,
    /// Layout map for the current frame (updated each draw).
    pub layout_map: LayoutMap,
    /// Show a brief "copied" toast.
    pub copy_toast_ticks: u8,
    pub copy_toast_message: String,
    pub copy_toast_failed: bool,
    /// Ticks remaining in which a second Ctrl+C quits.
    pub ctrl_c_armed_ticks: u8,
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
}

struct UiRuntime {
    current_provider: Arc<Mutex<String>>,
    current_model: Arc<Mutex<String>>,
    harness: Arc<Mutex<HarnessSnapshot>>,
    activity_status: Arc<Mutex<String>>,
    pending_permission: Arc<Mutex<Option<PermissionRequest>>>,
    is_responding: Arc<AtomicBool>,
    messages: Arc<Mutex<Vec<ChatMessage>>>,
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

    fn suggestion_matches(&self) -> Vec<(&str, &str)> {
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
            .copied()
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
            .copied()
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
            .copied()
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
            .copied()
            .collect();
        }

        if let Some(after) = current.strip_prefix("/session ") {
            return [
                ("/session status", "Show session id and loop checkpoint"),
                ("/session list", "List durable session branches"),
                ("/session resume", "Resume the most recent cached session"),
                ("/session fork", "Fork the current conversation"),
                ("/session new", "Start a new durable session"),
            ]
            .iter()
            .filter(|(cmd, _)| {
                cmd.strip_prefix("/session ")
                    .map(|sub| sub.starts_with(after))
                    .unwrap_or(false)
            })
            .copied()
            .collect();
        }

        SLASH_COMMANDS
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(&current))
            .copied()
            .chain(
                self.custom_commands
                    .iter()
                    .filter(|(command, _)| command.starts_with(&current))
                    .map(|(command, description)| (command.as_str(), description.as_str())),
            )
            .collect()
    }
}

pub async fn run_tui(
    tx: mpsc::UnboundedSender<AgentRequest>,
    mut rx: mpsc::UnboundedReceiver<AgentResponse>,
    initial_provider: String,
    initial_model: String,
    input_history: Vec<String>,
    initial_messages: Vec<Message>,
    custom_commands: Vec<(String, String)>,
) -> Result<Vec<String>, Box<dyn Error>> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.show_cursor()?;

    let restored = chat_messages_from_core(initial_messages);
    let messages = Arc::new(Mutex::new(restored));
    let messages_clone = messages.clone();
    let should_quit = Arc::new(AtomicBool::new(false));
    let should_quit_clone = should_quit.clone();

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

    // Spawn response listener
    tokio::spawn(async move {
        let mut reasoning_start: Option<std::time::Instant> = None;
        while let Some(resp) = rx.recv().await {
            match resp {
                AgentResponse::Text(t) => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(ChatMessage::new(Role::Assistant, t));
                    ir_clone.store(false, Ordering::SeqCst);
                    activity_clone.lock().await.clear();
                }
                AgentResponse::Activity(status) => {
                    *activity_clone.lock().await = status;
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::StreamStart => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(ChatMessage::new(Role::Assistant, ""));
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
                        msgs.push(ChatMessage::thinking(delta));
                        reasoning_start = Some(std::time::Instant::now());
                    }
                }
                AgentResponse::StreamReasoningEnd(content) => {
                    if let Some(started) = reasoning_start.take() {
                        let duration_ms = started.elapsed().as_millis() as u64;
                        let mut msgs = messages_clone.lock().await;
                        if let Some(last) = msgs.last_mut().filter(|message| message.is_thinking())
                        {
                            last.raw = content.clone();
                            last.reparse();
                            if let MessageKind::Thinking {
                                content: current,
                                duration_ms: d,
                                ..
                            } = &mut last.kind
                            {
                                *current = content;
                                *d = Some(duration_ms);
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
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(ChatMessage::tool_step(id, name, arguments));
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::ToolResult {
                    id,
                    name,
                    output,
                    duration_ms,
                } => {
                    *activity_clone.lock().await = "thinking".to_string();
                    let mut msgs = messages_clone.lock().await;
                    if !msgs
                        .iter_mut()
                        .any(|message| message.finish_tool_step(&id, output.clone(), duration_ms))
                    {
                        let mut message = ChatMessage::tool_step(id.clone(), name.clone(), "{}");
                        message.finish_tool_step(&id, output, duration_ms);
                        msgs.push(message);
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
                    *messages_clone.lock().await = chat_messages_from_core(messages);
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
                    messages_clone.lock().await.push(ChatMessage::new(
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
                    msgs.push(ChatMessage::new(Role::System, format!("Error: {}", e)));
                    ir_clone.store(false, Ordering::SeqCst);
                    activity_clone.lock().await.clear();
                }
                AgentResponse::Exit => {
                    should_quit_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::ProviderSwitched { provider, model } => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(ChatMessage::new(
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
        sticky_card: None,
        sticky_rect: None,
        tx,
        should_quit,
        suggestion_index: None,
        custom_commands,
        cursor_position: 0,
        active_modal: Modal::None,
        modal_index: 0,
        current_provider: initial_provider,
        current_model: initial_model,
        current_mode: AgentMode::Build,
        current_goal: None,
        loop_status: "idle".to_string(),
        activity_status: String::new(),
        pending_permission: None,
        sessions_overview: Vec::new(),
        permission_confirm_always: false,
        input_history,
        history_index: None,
        selection: SelectionState::None,
        drag: SelectionDrag::default(),
        layout_map: LayoutMap::new(),
        copy_toast_ticks: 0,
        copy_toast_message: String::new(),
        copy_toast_failed: false,
        ctrl_c_armed_ticks: 0,
        stashed_input: String::new(),
        setup_solution: None,
        setup_endpoint: None,
        setup_model: None,
        key_status: HashMap::new(),
        theme: Theme::default(),
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
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        return Err(err.into());
    }

    Ok(app.input_history)
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

    loop {
        if app.should_quit.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Apply any completed background clipboard copies.
        while let Ok(result) = copy_rx.try_recv() {
            set_copy_feedback(app, result);
            app.copy_toast_ticks = 5;
        }

        // Sync provider/model from listener
        {
            app.current_provider = runtime.current_provider.lock().await.clone();
            app.current_model = runtime.current_model.lock().await.clone();
            let harness = runtime.harness.lock().await.clone();
            app.current_mode = harness.mode;
            app.current_goal = harness.goal;
            app.loop_status = harness.loop_status;
            app.activity_status = runtime.activity_status.lock().await.clone();
            app.pending_permission = runtime.pending_permission.lock().await.clone();
            app.key_status = runtime.key_status.lock().await.clone();
            if app.pending_permission.is_some() && app.active_modal == Modal::None {
                app.active_modal = Modal::Permission;
                app.modal_index = 0;
            } else if app.pending_permission.is_none() && app.active_modal == Modal::Permission {
                app.active_modal = Modal::None;
                app.modal_index = 0;
                app.permission_confirm_always = false;
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
        if app.copy_toast_ticks > 0 {
            app.copy_toast_ticks -= 1;
        }
        if app.ctrl_c_armed_ticks > 0 {
            app.ctrl_c_armed_ticks -= 1;
        }

        // Pull messages from the shared lock into app state for rendering
        app.messages = runtime.messages.lock().await.clone();

        // While following, keep the newest content in view using the previous
        // frame's measurement (max_scroll is recomputed after each draw).
        if app.follow_bottom {
            app.scroll = app.max_scroll;
        }

        // Draw frame
        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            let status = display_status(
                &app.loop_status,
                &app.activity_status,
                app.pending_permission.is_some(),
            );

            let chat_render = render::draw_chat(
                f,
                &mut layout_map,
                render::ChatView {
                    messages: &app.messages,
                    scroll: app.scroll,
                    selection: &app.selection,
                    current_provider: &app.current_provider,
                    current_model: &app.current_model,
                    current_mode: app.current_mode,
                    current_goal: app.current_goal.as_ref(),
                    activity: &status,
                    theme: &app.theme,
                },
            );
            let input_rect = chat_render.input_rect;
            app.content_lines = chat_render.content_lines;
            app.view_height = chat_render.view_height;
            match chat_render.sticky {
                Some(info) => {
                    app.sticky_card = Some(info.message_idx);
                    app.sticky_rect = Some(info.rect);
                }
                None => {
                    app.sticky_card = None;
                    app.sticky_rect = None;
                }
            }

            let masked_input = if app.active_modal == Modal::ApiKey {
                "•".repeat(app.input.chars().count())
            } else {
                app.input.clone()
            };
            let accent = match app.current_mode {
                AgentMode::Plan => ratatui::style::Color::Rgb(137, 180, 250),
                AgentMode::Build => app.theme.accent,
            };
            render::draw_input(
                f,
                input_rect,
                &masked_input,
                app.cursor_display_x(),
                accent,
                &app.theme,
            );

            // Right-aligned hint line below the input box.
            let hint_rect = Rect::new(
                input_rect.x,
                input_rect.y + input_rect.height,
                input_rect.width,
                1,
            );
            render::draw_hint(
                f,
                hint_rect,
                &[
                    ("ctrl+p", "commands"),
                    ("ctrl+h", "help"),
                    ("enter", "send"),
                ],
                &app.theme,
            );

            // Slash suggestions
            if app.active_modal == Modal::None && app.input.starts_with('/') {
                let suggestions = app.suggestion_matches();
                if !suggestions.is_empty() {
                    render::draw_suggestions(
                        f,
                        &mut layout_map,
                        &suggestions,
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
                    render::draw_history_modal(
                        f,
                        &mut layout_map,
                        &app.input_history,
                        app.modal_index,
                        &app.theme,
                    );
                }
                Modal::Permission => {
                    if let Some(request) = app.pending_permission.as_ref() {
                        render::draw_permission_sheet(
                            f,
                            request,
                            app.modal_index,
                            app.permission_confirm_always,
                            &app.theme,
                        );
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
                Modal::Sessions => render::draw_sessions_modal(
                    f,
                    &app.sessions_overview,
                    app.modal_index.min(app.sessions_overview.len().saturating_sub(1)),
                    &app.theme,
                ),
                Modal::None => {}
            }

            // Copy toast
            if app.copy_toast_ticks > 0 {
                render::draw_copy_toast(
                    f,
                    &app.copy_toast_message,
                    app.copy_toast_failed,
                    &app.theme,
                );
            }
            if app.ctrl_c_armed_ticks > 0 {
                render::draw_exit_toast(f, &app.theme);
            }

            app.layout_map = layout_map;
        })?;

        // Recompute the bottom scroll offset for the next frame and keep the
        // manual scroll position within bounds when not following.
        app.max_scroll = app.content_lines.saturating_sub(app.view_height as usize) as u16;
        if !app.follow_bottom {
            app.scroll = app.scroll.min(app.max_scroll);
        }

        if event::poll(std::time::Duration::from_millis(100))? {
            let event = event::read()?;
            // Pre-compute suggestion data to avoid borrow conflicts with process_event.
            let suggestions = app.suggestion_matches();
            let suggestion_count = suggestions.len();
            let has_exact_suggestion = suggestions.iter().any(|(command, _)| *command == app.input);
            let input_starts_with_slash = app.input.starts_with('/');
            let action = input::process_event(
                event,
                &mut app.input,
                &mut app.cursor_position,
                input::InputContext {
                    active_modal: app.active_modal,
                    is_responding: runtime.is_responding.load(Ordering::SeqCst),
                    input_starts_with_slash,
                    suggestion_count,
                    has_exact_suggestion,
                    suggestion_index: app.suggestion_index,
                    permission_confirm_always: app.permission_confirm_always,
                },
                &mut app.drag,
            );

            match action {
                input::InputAction::None => {}
                input::InputAction::Quit => return Ok(()),
                input::InputAction::SendChat(text) => {
                    let text = if text.is_empty() && app.active_modal == Modal::HistorySearch {
                        // History search selection
                        app.input_history
                            .get(app.modal_index)
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        text
                    };

                    app.active_modal = Modal::None;
                    app.suggestion_index = None;

                    if !text.is_empty() {
                        runtime.is_responding.store(true, Ordering::SeqCst);
                        *runtime.activity_status.lock().await = "queued".to_string();
                        runtime
                            .messages
                            .lock()
                            .await
                            .push(ChatMessage::new(Role::User, text.clone()));
                        if app.input_history.last() != Some(&text) {
                            app.input_history.push(text.clone());
                        }
                        app.history_index = None;
                        app.follow_bottom = true;
                        let _ = app.tx.send(AgentRequest::Chat(text));
                    } else if let Some((start, end)) = app.selection.normalized_range() {
                        // Enter on a selected tool-step or thinking card toggles just that card.
                        if start.message_idx == end.message_idx {
                            let mi = start.message_idx;
                            let mut messages = runtime.messages.lock().await;
                            if let Some(message) = messages.get_mut(mi) {
                                if let Some(expanded) = message.tool_step_expanded() {
                                    message.set_tool_step_expanded(!expanded);
                                    app.selection = SelectionState::None;
                                } else if let Some(expanded) = message.thinking_expanded() {
                                    message.set_thinking_expanded(!expanded);
                                    app.selection = SelectionState::None;
                                }
                            }
                        }
                    }
                }
                input::InputAction::SendSlash(cmd) => {
                    app.suggestion_index = None;
                    runtime.is_responding.store(true, Ordering::SeqCst);
                    *runtime.activity_status.lock().await = "queued".to_string();
                    app.follow_bottom = true;
                    runtime
                        .messages
                        .lock()
                        .await
                        .push(ChatMessage::new(Role::User, cmd.clone()));
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
                    let _ = app.tx.send(AgentRequest::Interrupt);
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
                    app.active_modal = Modal::HistorySearch;
                    app.modal_index = app.input_history.len().saturating_sub(1);
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
                    if let Some(session) = app
                        .sessions_overview
                        .get(app.modal_index.min(app.sessions_overview.len().saturating_sub(1)))
                    {
                        let id = session.id.clone();
                        app.active_modal = Modal::None;
                        app.modal_index = 0;
                        let _ = app
                            .tx
                            .send(AgentRequest::SlashCommand(format!("/session open {}", id)));
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
                    }
                    app.active_modal = Modal::None;
                }
                input::InputAction::ScrollUp => {
                    app.follow_bottom = false;
                    if app.scroll > 0 {
                        app.scroll -= 1;
                    }
                }
                input::InputAction::ScrollDown => {
                    if app.scroll < app.max_scroll {
                        app.scroll += 1;
                    }
                    if app.scroll >= app.max_scroll {
                        app.follow_bottom = true;
                    }
                }
                input::InputAction::CopySelection => {
                    if let Some(text) = get_selected_text(&app.selection, &app.messages) {
                        spawn_clipboard_copy(&copy_tx, text);
                    }
                }
                input::InputAction::CtrlC => {
                    if let Some(text) = get_selected_text(&app.selection, &app.messages) {
                        spawn_clipboard_copy(&copy_tx, text);
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
                    } else if app.active_modal != Modal::None
                        && app.active_modal != Modal::Permission
                    {
                        app.active_modal = Modal::None;
                    } else if runtime.is_responding.load(Ordering::SeqCst) {
                        let _ = app.tx.send(AgentRequest::Interrupt);
                    } else if !app.input.is_empty() {
                        app.input.clear();
                        app.cursor_position = 0;
                        app.suggestion_index = None;
                    } else if app.ctrl_c_armed_ticks > 0 {
                        return Ok(());
                    } else {
                        // Arm a ~2s window in which a second Ctrl+C quits.
                        app.ctrl_c_armed_ticks = 20;
                    }
                }
                input::InputAction::ToggleToolSteps => {
                    let mut messages = runtime.messages.lock().await;
                    let expand = messages
                        .iter()
                        .any(|message| message.tool_step_expanded() == Some(false));
                    for message in messages.iter_mut() {
                        message.set_tool_step_expanded(expand);
                    }
                    app.selection = SelectionState::None;
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
                    let count = app.suggestion_matches().len();
                    if count > 0 {
                        let next = match app.suggestion_index {
                            Some(i) => (i + 1) % count,
                            None => 0,
                        };
                        app.suggestion_index = Some(next);
                    }
                }
                input::InputAction::SuggestPrev => {
                    let count = app.suggestion_matches().len();
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
                        let cmds: Vec<_> =
                            app.suggestion_matches().iter().map(|(c, _)| *c).collect();
                        if let Some(cmd) = cmds.get(idx) {
                            app.input = cmd.to_string();
                            app.cursor_position = app.input.chars().count();
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
                        app.modal_index = if app.modal_index == 0 {
                            app.input_history.len().saturating_sub(1)
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
                    | Modal::None => {}
                },
                input::InputAction::ModalDown => match app.active_modal {
                    Modal::Models => {
                        app.modal_index = (app.modal_index + 1) % SOLUTIONS.len();
                    }
                    Modal::HistorySearch => {
                        app.modal_index = (app.modal_index + 1) % app.input_history.len().max(1);
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
                    | Modal::None => {}
                },
                input::InputAction::PermissionSubmit => {
                    if app.permission_confirm_always {
                        if app.modal_index == 1 {
                            app.permission_confirm_always = false;
                            app.modal_index = 1;
                            continue;
                        }
                    } else if app.modal_index == 1 {
                        app.permission_confirm_always = true;
                        app.modal_index = 0;
                        continue;
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
                    // Sticky pinned card header: collapse it on click.
                    if app
                        .sticky_rect
                        .is_some_and(|r| r.x <= x && x < r.x + r.width && r.y <= y && y < r.y + r.height)
                    {
                        if let Some(mi) = app.sticky_card {
                            let mut messages = runtime.messages.lock().await;
                            if let Some(message) = messages.get_mut(mi) {
                                if let Some(expanded) = message.tool_step_expanded() {
                                    message.set_tool_step_expanded(!expanded);
                                } else if let Some(expanded) = message.thinking_expanded() {
                                    message.set_thinking_expanded(!expanded);
                                }
                            }
                        }
                        app.selection = SelectionState::None;
                        app.drag.cancel();
                    } else if let Some(cursor) = input::resolve_cursor(&app.layout_map, x, y) {
                        if cursor.block_idx == usize::MAX {
                            // Clicked a tool-step card header: toggle that card.
                            let mut messages = runtime.messages.lock().await;
                            if let Some(message) = messages.get_mut(cursor.message_idx) {
                                if let Some(expanded) = message.tool_step_expanded() {
                                    message.set_tool_step_expanded(!expanded);
                                }
                            }
                            app.selection = SelectionState::None;
                            app.drag.cancel();
                        } else if cursor.block_idx == usize::MAX - 1 {
                            // Clicked a thinking card header: toggle that card.
                            let mut messages = runtime.messages.lock().await;
                            if let Some(message) = messages.get_mut(cursor.message_idx) {
                                if let Some(expanded) = message.thinking_expanded() {
                                    message.set_thinking_expanded(!expanded);
                                }
                            }
                            app.selection = SelectionState::None;
                            app.drag.cancel();
                        } else {
                            app.selection = SelectionState::start_range(cursor);
                            app.drag.start(cursor);
                        }
                    } else {
                        app.selection = SelectionState::None;
                        app.drag.cancel();
                    }
                }
                input::InputAction::SelectionUpdate { x, y } => {
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

fn spawn_clipboard_copy(
    tx: &mpsc::UnboundedSender<Result<clipboard::CopyOutcome, String>>,
    text: String,
) {
    let tx = tx.clone();
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
    });
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
) -> Result<Vec<String>, Box<dyn Error>> {
    run_tui(
        tx,
        rx,
        initial_provider,
        initial_model,
        input_history,
        initial_messages,
        custom_commands,
    )
    .await
}

fn chat_message_from_core(message: Message) -> Option<ChatMessage> {
    if message.hidden || message.role == Role::System {
        return None;
    }
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
        Some(ChatMessage::new(message.role, content))
    }
}

fn chat_messages_from_core(messages: Vec<Message>) -> Vec<ChatMessage> {
    let mut restored = Vec::new();
    for mut message in messages {
        if message.hidden || message.role == Role::System {
            continue;
        }
        if message.role == Role::Assistant {
            if let Some(reasoning) = message.reasoning_content.take() {
                restored.push(ChatMessage::thinking(reasoning));
            }
            if let Some(calls) = message.tool_calls.take() {
                for call in calls {
                    // Historical results match by tool name, so use it as the id.
                    restored.push(ChatMessage::tool_step(
                        call.name.clone(),
                        call.name,
                        call.arguments,
                    ));
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
                    .any(|item| item.finish_tool_step(name, output, 0))
                {
                    continue;
                }
            }
        }
        if let Some(message) = chat_message_from_core(message) {
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
        assert!(chat_message_from_core(Message::hidden(Role::User, "internal")).is_none());
        assert!(chat_message_from_core(Message::new(Role::System, "system")).is_none());
    }

    #[test]
    fn restored_history_uses_command_display_content() {
        let message = Message::new(Role::User, "Expanded internal prompt")
            .with_display_content("/review working-tree");
        let restored = chat_message_from_core(message).unwrap();
        assert_eq!(restored.raw, "/review working-tree");
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
            hidden: false,
        };

        let restored = chat_message_from_core(message).unwrap();
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

        let mut restored = chat_messages_from_core(messages);
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
}
