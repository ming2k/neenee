//! Raw-mode / alternate-screen lifecycle: graceful cleanup, signal-guard.

use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags};
use crossterm::{execute, terminal::disable_raw_mode};

use std::io::{self, Write};

/// Undo raw mode, leave the alternate screen, and turn off mouse tracking.
/// Used both on graceful shutdown and from the signal guard so an externally
/// killed process (e.g. `pkill neenee`) does not strand the terminal in a
/// state where every mouse move spews SGR escape codes into the shell.
pub(super) fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = stdout.flush();
}

/// Catch termination signals and restore the terminal before exiting. Without
/// this, SIGTERM/SIGHUP (as sent by `pkill`) terminates the process without
/// running `run_tui`'s normal cleanup, leaving the host terminal in raw mode
/// with mouse capture enabled.
pub(super) fn spawn_signal_guard() {
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
