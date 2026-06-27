//! Standalone UI component showcase — a "Storybook" for TUI components.
//!
//! `neenee showcase <component>` renders a single UI component in isolation
//! in a real terminal, wired to a live event loop so you can **see and
//! interact with it** without running the full agent/session/network stack.
//!
//! Each sub-module is one showcase: it owns a model and pumps real crossterm
//! keypresses through it, redrawing via the production renderers (shared
//! verbatim). The shared terminal setup/teardown + event helpers live here in
//! [`common`].

pub mod common;
mod permission;
mod question;
mod simple;
mod tool_step;
mod transcript;

use std::io;

/// Entry point: dispatch to the showcase for `component`.
///
/// Returns `Ok(())` on a clean exit (Esc / submit / cancel) or an error
/// if the terminal setup/teardown failed.
pub fn run(component: &str) -> Result<(), Box<dyn std::error::Error>> {
    match component {
        "question" => question::run().map_err(Into::into),
        "permission" => permission::run().map_err(Into::into),
        "tool-step" => tool_step::run().map_err(Into::into),
        "transcript" => transcript::run().map_err(Into::into),
        "provider" => simple::provider().map_err(Into::into),
        "model-editor" => simple::model_editor().map_err(Into::into),
        "history" => simple::history().map_err(Into::into),
        "sessions" => simple::sessions().map_err(Into::into),
        "session" => simple::session().map_err(Into::into),
        "activity" => simple::activity().map_err(Into::into),
        "help" => simple::help().map_err(Into::into),
        "toast" => simple::toast().map_err(Into::into),
        _ => {
            let _ = io::Write::flush(&mut io::stdout());
            eprintln!(
                "Unknown showcase component '{component}'.\n\n\
                 Available showcases:\n  \
                 question     the ask_user multi-choice / free-text modal\n  \
                 permission   the tool-permission sheet (inline)\n  \
                 tool-step    the parallel tools transcript (spacing + lifecycles)\n  \
                 transcript   full transcript fixtures (markdown, CJK, scroll, resize)\n  \
                 provider     the /provider model picker\n  \
                 model-editor the API-key / model-id editor\n  \
                 history      the Ctrl+R input-history search\n  \
                 sessions     the session picker\n  \
                 session      the session-context tabbed modal\n  \
                 activity     the activity / pursuit / tasks modal\n  \
                 help         the keybindings help modal\n  \
                 toast        copy / armed toasts\n\n\
                 Usage: neenee showcase <component>"
            );
            std::process::exit(2);
        }
    }
}
