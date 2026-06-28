//! Shared showcase helpers: terminal lifecycle + a generic key-loop runner.
//!
//! Every showcase boils down to the same shape — own some state, pump real
//! keypresses into a closure that updates it, redraw via a render closure.
//! [`run_showcase`] captures that shape so each showcase file is just its
//! fixture data + two small closures (update + render).

use std::io::{self, Stdout, Write};
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use neenee_tui::{Backend, Frame, Terminal};

pub type Term = Terminal<Stdout>;

/// Set up a raw-mode alternate screen and return a ready terminal. Stripped
/// down from the real app's lifecycle (no bracketed paste / Kitty protocol),
/// but mouse capture is enabled so scrollable showcases can receive wheel
/// events.
pub fn setup_terminal() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    queue!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    stdout.flush()?;
    let backend = Backend::new(stdout);
    Ok(Terminal::new(backend))
}

/// Restore the terminal (disable raw mode, leave alternate screen, show cursor).
pub fn restore_terminal(terminal: &mut Term) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.writer(), DisableMouseCapture, LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// A decoded key event the showcase closures consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowKey {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

/// A decoded terminal event the showcase closures consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShowEvent {
    Key(ShowKey),
    Click { x: u16, y: u16 },
    ScrollUp,
    ScrollDown,
}

/// The decision a per-key handler returns: keep looping, or stop.
pub enum ShowAction {
    /// Continue the loop (state may have changed; will redraw).
    Continue,
    /// Stop the loop and exit the showcase cleanly.
    Exit,
}

/// Run a showcase event loop. `render` is called every frame with shared
/// state; `on_key` is called for each real keypress and decides whether to
/// continue or exit. `Ctrl+C` and bare `q` always exit (the global kill
/// switch) so a showcase can never trap you in a raw-mode terminal.
///
/// The shared `state` is passed by `&mut` to *both* closures so they don't
/// each capture overlapping borrows of the surrounding locals (which would
/// fight the borrow checker). Each showcase declares a small state struct
/// holding its mutable bits.
pub fn run_showcase<S, R, H>(state: &mut S, render: R, mut on_key: H) -> io::Result<()>
where
    R: FnMut(&mut Frame, &S),
    H: FnMut(&mut S, ShowKey) -> ShowAction,
{
    run_showcase_events(state, render, |state, event| match event {
        ShowEvent::Key(key) => on_key(state, key),
        ShowEvent::Click { .. } | ShowEvent::ScrollUp | ShowEvent::ScrollDown => {
            ShowAction::Continue
        }
    })
}

/// Run a showcase event loop with both keyboard and mouse-wheel input.
pub fn run_showcase_events<S, R, H>(state: &mut S, mut render: R, mut on_event: H) -> io::Result<()>
where
    R: FnMut(&mut Frame, &S),
    H: FnMut(&mut S, ShowEvent) -> ShowAction,
{
    let mut terminal = setup_terminal()?;

    loop {
        terminal.draw(|f| render(f, state))?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) => {
                if kind == KeyEventKind::Release {
                    continue;
                }

                // Global exits: Ctrl+C and bare 'q' always work.
                if (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
                    || (code == KeyCode::Char('q') && modifiers.is_empty())
                {
                    break;
                }

                if matches!(
                    on_event(state, ShowEvent::Key(ShowKey { code, modifiers })),
                    ShowAction::Exit
                ) {
                    break;
                }
            }
            Event::Mouse(mouse) => {
                let event = match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => Some(ShowEvent::Click {
                        x: mouse.column,
                        y: mouse.row,
                    }),
                    MouseEventKind::ScrollUp => Some(ShowEvent::ScrollUp),
                    MouseEventKind::ScrollDown => Some(ShowEvent::ScrollDown),
                    _ => None,
                };
                if let Some(event) = event
                    && matches!(on_event(state, event), ShowAction::Exit)
                {
                    break;
                }
            }
            Event::Resize(width, height) => {
                terminal.resize_to(width, height);
            }
            _ => {}
        }
    }

    restore_terminal(&mut terminal)?;
    Ok(())
}

/// Paint the full terminal surface with the app background before a showcase
/// draws partial chrome or a modal. The production transcript renderer does
/// this itself; standalone modal showcases need the same ownership so resize
/// cannot leave retained old cells behind.
pub fn draw_app_background(f: &mut Frame, theme: &crate::tui::render::Theme) {
    use neenee_tui::{Block, Style};

    f.render_widget(
        Block::default().style(Style::default().bg(theme.surface())),
        f.area(),
    );
}

/// Helper to draw a fixed 3-row chrome around a centered modal: a title header
/// (top), the modal body (flex), and a hint footer (bottom). Showcases call
/// this so they all share the same framing — only the inner renderer differs.
pub fn draw_with_chrome<F>(
    f: &mut Frame,
    title: &str,
    hint: &str,
    theme: &crate::tui::render::Theme,
    draw_modal: F,
) where
    F: FnOnce(&mut Frame),
{
    use neenee_tui::{Block, Borders, Paragraph};
    use neenee_tui::{Constraint, Direction, Layout};
    use neenee_tui::{Line, Span};
    use neenee_tui::{Modifier, Style};

    draw_app_background(f, theme);

    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let header = Block::default().borders(Borders::BOTTOM);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )))
        .block(header),
        chunks[0],
    );

    draw_modal(f);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(theme.muted()),
        ))),
        chunks[2],
    );
}
