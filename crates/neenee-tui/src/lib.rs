use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Terminal,
};
use std::{error::Error, io, sync::Arc, sync::atomic::{AtomicBool, Ordering}};
use tokio::sync::{mpsc, Mutex};
use neenee_core::{AgentRequest, AgentResponse};
use unicode_width::UnicodeWidthStr;

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/models", "List available LLM backends"),
    ("/mode", "Switch between Build and Plan modes"),
    ("/exit", "Gracefully exit the program"),
    ("/help", "Show available commands and usage"),
    ("/clear", "Clear the conversation history"),
];

#[derive(PartialEq)]
pub enum Modal {
    None,
    Models,
    HistorySearch,
}

pub struct App {
    pub input: String,
    pub messages: Arc<Mutex<Vec<String>>>,
    pub scroll: u16,
    pub tx: mpsc::UnboundedSender<AgentRequest>,
    pub should_quit: Arc<AtomicBool>,
    pub suggestion_index: Option<usize>,
    pub cursor_position: usize,
    pub active_modal: Modal,
    pub modal_index: usize,
    pub current_provider: String,
    pub current_model: String,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
}

impl App {
    pub fn byte_cursor(&self) -> usize {
        self.input
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.cursor_position)
            .unwrap_or(self.input.len())
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

const PROVIDERS: &[(&str, &str, &str)] = &[
    ("OpenAI", "gpt-4o", "OpenAI API"),
    ("Gemini", "gemini-1.5-flash", "Google Gemini"),
    ("Llama", "local-model", "Local Llama Server"),
    ("Mock", "mock-model", "Test Provider"),
];

pub async fn run_tui(
    tx: mpsc::UnboundedSender<AgentRequest>,
    mut rx: mpsc::UnboundedReceiver<AgentResponse>,
    initial_provider: String,
    initial_model: String,
    input_history: Vec<String>,
) -> Result<Vec<String>, Box<dyn Error>> {
    // setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.show_cursor()?;

    let messages = Arc::new(Mutex::new(Vec::new()));
    let messages_clone = messages.clone();
    let should_quit = Arc::new(AtomicBool::new(false));
    let should_quit_clone = should_quit.clone();
    
    let current_provider = Arc::new(Mutex::new(initial_provider.clone()));
    let current_model = Arc::new(Mutex::new(initial_model.clone()));
    let cp_clone = current_provider.clone();
    let cm_clone = current_model.clone();
    
    let is_responding = Arc::new(AtomicBool::new(false));
    let ir_clone = is_responding.clone();

    // Spawn response listener
    tokio::spawn(async move {
        while let Some(resp) = rx.recv().await {
            match resp {
                AgentResponse::Text(t) => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(format!("AI: {}", t));
                    ir_clone.store(false, Ordering::SeqCst);
                }
                AgentResponse::StreamStart => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push("AI: ".to_string());
                    ir_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::StreamDelta(delta) => {
                    let mut msgs = messages_clone.lock().await;
                    if let Some(last) = msgs.last_mut() {
                        last.push_str(&delta);
                    }
                }
                AgentResponse::StreamEnd => {
                    ir_clone.store(false, Ordering::SeqCst);
                }
                AgentResponse::Error(e) => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(format!("Error: {}", e));
                    ir_clone.store(false, Ordering::SeqCst);
                }
                AgentResponse::Exit => {
                    should_quit_clone.store(true, Ordering::SeqCst);
                }
                AgentResponse::ProviderSwitched { provider, model } => {
                    let mut msgs = messages_clone.lock().await;
                    msgs.push(format!("System: Provider switched to {} ({})", provider, model));
                    *cp_clone.lock().await = provider;
                    *cm_clone.lock().await = model;
                }
            }
        }
    });

    let mut app = App {
        input: String::new(),
        messages,
        scroll: 0,
        tx,
        should_quit,
        suggestion_index: None,
        cursor_position: 0,
        active_modal: Modal::None,
        modal_index: 0,
        current_provider: initial_provider,
        current_model: initial_model,
        input_history,
        history_index: None,
    };

    // run app
    let res = run_app_loop(&mut terminal, &mut app, current_provider, current_model, is_responding).await;

    // restore terminal
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

use pulldown_cmark::{Event as MdEvent, Parser, Tag, TagEnd};

fn parse_markdown(text: &str) -> Vec<Line<'_>> {
    let parser = Parser::new(text);
    let mut lines = Vec::new();
    let mut current_line = Vec::new();
    let mut styles = Vec::new();

    for event in parser {
        match event {
            MdEvent::Start(tag) => match tag {
                Tag::Emphasis => styles.push(Style::default().add_modifier(Modifier::ITALIC)),
                Tag::Strong => styles.push(Style::default().add_modifier(Modifier::BOLD)),
                Tag::CodeBlock(_) => styles.push(Style::default().fg(Color::Cyan).bg(Color::Rgb(30, 30, 30))),
                Tag::Heading { .. } => styles.push(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                Tag::Link { .. } => styles.push(Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED)),
                _ => {}
            },
            MdEvent::End(tag) => match tag {
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::CodeBlock | TagEnd::Heading(_) | TagEnd::Link => {
                    styles.pop();
                }
                _ => {}
            },
            MdEvent::Text(t) => {
                let mut style = Style::default();
                for s in &styles {
                    style = style.patch(*s);
                }
                current_line.push(Span::styled(t.to_string(), style));
            }
            MdEvent::Code(t) => {
                current_line.push(Span::styled(
                    format!(" {} ", t),
                    Style::default().fg(Color::Yellow).bg(Color::Rgb(40, 44, 52)),
                ));
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                lines.push(Line::from(std::mem::take(&mut current_line)));
            }
            MdEvent::Rule => {
                lines.push(Line::from(Span::styled("───", Style::default().fg(Color::DarkGray))));
            }
            _ => {}
        }
    }

    if !current_line.is_empty() {
        lines.push(Line::from(current_line));
    }

    lines
}

async fn run_app_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    current_provider_mu: Arc<Mutex<String>>,
    current_model_mu: Arc<Mutex<String>>,
    is_responding: Arc<AtomicBool>,
) -> io::Result<()> {
    loop {
        if app.should_quit.load(Ordering::SeqCst) {
            return Ok(());
        }
        
        // Sync provider/model from listener
        {
            app.current_provider = current_provider_mu.lock().await.clone();
            app.current_model = current_model_mu.lock().await.clone();
        }

        let msgs = app.messages.lock().await.clone();
        terminal.draw(|f| {
            // Simple layout: Chat area + Input area
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // Header
                    Constraint::Min(0),    // Chat
                    Constraint::Length(1), // Separator
                    Constraint::Length(1), // Input
                ])
                .split(f.size());

            // 1. Header
            let header = Line::from(vec![
                Span::styled(" ● ", Style::default().fg(Color::Green)),
                Span::styled("neenee", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" — "),
                Span::styled(format!("{} ({})", app.current_provider, app.current_model), Style::default().fg(Color::Cyan)),
                Span::raw(" — "),
                Span::styled("Integrated Agent", Style::default().fg(Color::DarkGray)),
            ]);
            f.render_widget(Paragraph::new(header), chunks[0]);

            // 2. Chat History
            let mut history_spans = Vec::new();
            for msg in &msgs {
                if msg.starts_with("You: ") {
                    history_spans.push(Line::from(vec![
                        Span::styled(" ❯ ", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
                        Span::styled(&msg[5..], Style::default().fg(Color::White)),
                    ]));
                    history_spans.push(Line::from("")); 
                } else if msg.starts_with("AI: ") {
                    history_spans.push(Line::from(vec![
                        Span::styled(" ◈ ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                        Span::styled("Assistant", Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)),
                    ]));
                    
                    let content = &msg[4..];
                    let markdown_lines = parse_markdown(content);
                    for line in markdown_lines {
                        let mut line_spans = vec![Span::raw("   ")]; // Indent AI response
                        line_spans.extend(line.spans);
                        history_spans.push(Line::from(line_spans));
                    }
                    history_spans.push(Line::from("")); 
                } else if msg.starts_with("Error: ") {
                    history_spans.push(Line::from(vec![
                        Span::styled(" ✖ ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                        Span::styled(&msg[7..], Style::default().fg(Color::Red)),
                    ]));
                    history_spans.push(Line::from("")); 
                } else {
                    history_spans.push(Line::from(Span::styled(msg, Style::default().fg(Color::DarkGray))));
                    history_spans.push(Line::from("")); 
                }
            }

            let body = Paragraph::new(history_spans)
                .wrap(Wrap { trim: true })
                .scroll((app.scroll, 0));
            f.render_widget(body, chunks[1]);

            // 3. Separator
            let separator = Paragraph::new(Span::styled("─".repeat(f.size().width as usize), Style::default().fg(Color::DarkGray)));
            f.render_widget(separator, chunks[2]);

            // 4. Input
            let input = Paragraph::new(Line::from(vec![
                Span::styled(" › ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(&app.input),
            ]));
            f.render_widget(input, chunks[3]);

            // Set the cursor at the current position
            let cursor_x = app.input[..app.byte_cursor()].width() as u16;
            f.set_cursor(
                chunks[3].x + cursor_x + 3,
                chunks[3].y,
            );

            // 5. Suggestions Popup
            if app.active_modal == Modal::None && app.input.starts_with('/') {
                let current_input = app.input.to_lowercase();
                let matches: Vec<_> = SLASH_COMMANDS
                    .iter()
                    .filter(|(cmd, _)| cmd.starts_with(&current_input))
                    .collect();

                if !matches.is_empty() {
                    let height = (matches.len() as u16).min(5);
                    let area = Rect::new(
                        chunks[3].x + 3,
                        chunks[3].y.saturating_sub(height),
                        45,
                        height,
                    );

                    f.render_widget(Clear, area); 
                    let items: Vec<ListItem> = matches
                        .iter()
                        .enumerate()
                        .map(|(i, (cmd, desc))| {
                            let style = if Some(i) == app.suggestion_index {
                                Style::default().bg(Color::DarkGray).fg(Color::Yellow)
                            } else {
                                Style::default().fg(Color::Gray)
                            };
                            ListItem::new(Line::from(vec![
                                Span::styled(format!(" {:<10} ", cmd), style.add_modifier(Modifier::BOLD)),
                                Span::styled(format!("│ {}", desc), Style::default().fg(Color::DarkGray)),
                            ]))
                        })
                        .collect();

                    let list = List::new(items)
                        .block(Block::default().borders(Borders::LEFT).border_style(Style::default().fg(Color::Yellow)));
                    f.render_widget(list, area);
                }
            }

            // 6. Models Modal
            if app.active_modal == Modal::Models {
                let area = centered_rect(80, 50, f.size());
                f.render_widget(Clear, area);
                
                let items: Vec<ListItem> = PROVIDERS
                    .iter()
                    .enumerate()
                    .map(|(i, (name, model, desc))| {
                        let is_current = name.to_lowercase() == app.current_provider.to_lowercase();
                        let style = if i == app.modal_index {
                            Style::default().bg(Color::Rgb(50, 50, 50)).fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::Gray)
                        };
                        let prefix = if is_current { " ● " } else { "   " };
                        ListItem::new(Line::from(vec![
                            Span::styled(prefix, if is_current { Style::default().fg(Color::Green) } else { style }),
                            Span::styled(format!("{:<10} ", name), style.add_modifier(Modifier::BOLD)),
                            Span::styled(format!("│ {} ", model), Style::default().fg(Color::Cyan)),
                            Span::styled(format!("│ {}", desc), Style::default().fg(Color::DarkGray)),
                        ]))
                    })
                    .collect();

                let list = List::new(items)
                    .block(Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Magenta))
                        .title(Span::styled(" Select Model Provider ", Style::default().add_modifier(Modifier::BOLD))));
                f.render_widget(list, area);
            }

            // 7. History Search Modal
            if app.active_modal == Modal::HistorySearch {
                let area = centered_rect(80, 60, f.size());
                f.render_widget(Clear, area);

                let items: Vec<ListItem> = app.input_history
                    .iter()
                    .enumerate()
                    .map(|(i, h)| {
                        let style = if i == app.modal_index {
                            Style::default().bg(Color::Rgb(50, 50, 50)).fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::Gray)
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(format!(" {:>3} ", i + 1), Style::default().fg(Color::DarkGray)),
                            Span::styled(h, style),
                        ]))
                    })
                    .collect();

                let list = List::new(items)
                    .block(Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow))
                        .title(Span::styled(" Chat History ", Style::default().add_modifier(Modifier::BOLD))));
                f.render_widget(list, area);
            }
        })?;

        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read()? {
                Event::Mouse(mouse) => {
                    if app.active_modal == Modal::None {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                if app.scroll > 0 {
                                    app.scroll -= 1;
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                app.scroll += 1;
                            }
                            _ => {}
                        }
                    }
                }
                Event::Key(key) => {
                    match key.code {
                        KeyCode::Esc => {
                            if app.active_modal != Modal::None {
                                app.active_modal = Modal::None;
                            } else if is_responding.load(Ordering::SeqCst) {
                                let _ = app.tx.send(AgentRequest::Interrupt);
                            }
                        }
                        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if app.active_modal == Modal::None {
                                app.active_modal = Modal::HistorySearch;
                                app.modal_index = app.input_history.len().saturating_sub(1);
                            }
                        }
                        KeyCode::Char('q') if app.input.is_empty() && app.active_modal == Modal::None => return Ok(()),
                        KeyCode::Enter => {
                            if app.active_modal == Modal::Models {
                                let (name, model, _) = PROVIDERS[app.modal_index];
                                let _ = app.tx.send(AgentRequest::SwitchProvider {
                                    provider_type: name.to_lowercase(),
                                    model: model.to_string(),
                                    api_key: None,
                                    base_url: None,
                                });
                                app.active_modal = Modal::None;
                            } else if app.active_modal == Modal::HistorySearch {
                                if let Some(h) = app.input_history.get(app.modal_index) {
                                    app.input = h.clone();
                                    app.cursor_position = app.input.chars().count();
                                }
                                app.active_modal = Modal::None;
                            } else {
                                let mut user_input = std::mem::take(&mut app.input);
                                app.cursor_position = 0;
                                
                                if let Some(idx) = app.suggestion_index {
                                    let matches: Vec<_> = SLASH_COMMANDS
                                        .iter()
                                        .filter(|(cmd, _)| cmd.starts_with(&user_input.to_lowercase()))
                                        .collect();
                                    if let Some(choice) = matches.get(idx) {
                                        user_input = choice.0.to_string();
                                    }
                                }

                                if user_input == "/models" {
                                    app.active_modal = Modal::Models;
                                    if let Some(idx) = PROVIDERS.iter().position(|(p, _, _)| p.to_lowercase() == app.current_provider.to_lowercase()) {
                                        app.modal_index = idx;
                                    }
                                    app.suggestion_index = None;
                                } else if !user_input.is_empty() {
                                    app.suggestion_index = None;
                                    app.messages.lock().await.push(format!("You: {}", user_input));
                                    
                                    // Add to history
                                    if app.input_history.last() != Some(&user_input) {
                                        app.input_history.push(user_input.clone());
                                    }
                                    app.history_index = None;

                                    if user_input.starts_with('/') {
                                        let _ = app.tx.send(AgentRequest::SlashCommand(user_input));
                                    } else {
                                        let _ = app.tx.send(AgentRequest::Chat(user_input));
                                    }
                                }
                            }
                        }
                        KeyCode::Tab => {
                            if app.active_modal == Modal::None && app.input.starts_with('/') {
                                let matches: Vec<_> = SLASH_COMMANDS
                                    .iter()
                                    .filter(|(cmd, _)| cmd.starts_with(&app.input.to_lowercase()))
                                    .collect();
                                
                                if !matches.is_empty() {
                                    let next = match app.suggestion_index {
                                        Some(i) => (i + 1) % matches.len(),
                                        None => 0,
                                    };
                                    app.suggestion_index = Some(next);
                                    if let Some(choice) = matches.get(next) {
                                        app.input = choice.0.to_string();
                                        app.cursor_position = app.input.chars().count();
                                    }
                                }
                            }
                        }
                        KeyCode::Char(c) => {
                            if app.active_modal == Modal::None {
                                app.input.insert(app.byte_cursor(), c);
                                app.cursor_position += 1;
                                app.suggestion_index = None;
                            }
                        }
                        KeyCode::Backspace => {
                            if app.active_modal == Modal::None && app.cursor_position > 0 {
                                app.cursor_position -= 1;
                                app.input.remove(app.byte_cursor());
                                app.suggestion_index = None;
                            }
                        }
                        KeyCode::Left => {
                            if app.active_modal == Modal::None && app.cursor_position > 0 {
                                app.cursor_position -= 1;
                            }
                        }
                        KeyCode::Right => {
                            if app.active_modal == Modal::None && app.cursor_position < app.input.chars().count() {
                                app.cursor_position += 1;
                            }
                        }
                        KeyCode::Up => {
                            if app.active_modal == Modal::Models {
                                app.modal_index = if app.modal_index == 0 { PROVIDERS.len() - 1 } else { app.modal_index - 1 };
                            } else if app.active_modal == Modal::HistorySearch {
                                app.modal_index = if app.modal_index == 0 { app.input_history.len().saturating_sub(1) } else { app.modal_index - 1 };
                            } else if app.input.starts_with('/') {
                                 let matches: Vec<_> = SLASH_COMMANDS
                                    .iter()
                                    .filter(|(cmd, _)| cmd.starts_with(&app.input.to_lowercase()))
                                    .collect();
                                if !matches.is_empty() {
                                    let next = match app.suggestion_index {
                                        Some(i) => if i == 0 { matches.len() - 1 } else { i - 1 },
                                        None => matches.len() - 1,
                                    };
                                    app.suggestion_index = Some(next);
                                }
                            } else if app.active_modal == Modal::None {
                                // History navigation
                                if !app.input_history.is_empty() {
                                    let new_idx = match app.history_index {
                                        Some(i) => if i == 0 { 0 } else { i - 1 },
                                        None => app.input_history.len() - 1,
                                    };
                                    app.history_index = Some(new_idx);
                                    app.input = app.input_history[new_idx].clone();
                                    app.cursor_position = app.input.chars().count();
                                }
                            }
                        }
                        KeyCode::Down => {
                            if app.active_modal == Modal::Models {
                                app.modal_index = (app.modal_index + 1) % PROVIDERS.len();
                            } else if app.active_modal == Modal::HistorySearch {
                                app.modal_index = (app.modal_index + 1) % app.input_history.len().max(1);
                            } else if app.input.starts_with('/') {
                                 let matches: Vec<_> = SLASH_COMMANDS
                                    .iter()
                                    .filter(|(cmd, _)| cmd.starts_with(&app.input.to_lowercase()))
                                    .collect();
                                if !matches.is_empty() {
                                    let next = match app.suggestion_index {
                                        Some(i) => (i + 1) % matches.len(),
                                        None => 0,
                                    };
                                    app.suggestion_index = Some(next);
                                }
                            } else if app.active_modal == Modal::None {
                                // History navigation
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
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

pub async fn start_tui(
    tx: mpsc::UnboundedSender<AgentRequest>,
    rx: mpsc::UnboundedReceiver<AgentResponse>,
    initial_provider: String,
    initial_model: String,
    input_history: Vec<String>,
) -> Result<Vec<String>, Box<dyn Error>> {
    run_tui(tx, rx, initial_provider, initial_model, input_history).await
}