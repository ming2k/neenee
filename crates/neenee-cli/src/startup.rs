//! CLI bootstrap helpers: arg parsing, startup-mode selection, tracing init,
//! and the slash-command vocabulary used to distinguish built-in commands
//! from user-defined ones.
//!
//! These are pure (or near-pure: `init_tracing` does touch the env / filesystem)
//! helpers that lived inline at the top of `main.rs` before being grouped here.
//! They have no dependence on `main.rs` state.

use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;

/// Built-in slash commands understood directly by the harness. Anything not in
/// this list is treated as a custom command (from the `[commands]` table or an
/// on-disk script) and dispatched via `neenee_tools::commands`.
pub const BUILTIN_COMMANDS: &[&str] = &[
    "models",
    "mode",
    "mcp",
    "permissions",
    "auto-approve",
    "stall-threshold",
    "verify-nudge",
    "session",
    "sessions",
    "resume",
    "compact",
    "pursue",
    "repeat",
    "init",
    "skills",
    "skill",
    "export",
    "clear",
    "help",
    "exit",
];

/// Split `/<name> <arguments>` into `(name_without_slash, arguments_trimmed)`.
/// A bare `/name` with no arguments yields an empty arguments string.
pub fn split_custom_command(input: &str) -> (&str, &str) {
    let input = input.trim();
    let split_at = input.find(char::is_whitespace).unwrap_or(input.len());
    let (name, arguments) = input.split_at(split_at);
    (name.trim_start_matches('/'), arguments.trim())
}

#[derive(Debug)]
pub enum StartupMode {
    Fresh,
    Resume(Option<String>),
    Picker,
    Doctor,
}

pub fn parse_args(args: Vec<String>) -> (StartupMode, Option<PathBuf>, bool) {
    let mut iter = args.into_iter().peekable();
    let mut project: Option<PathBuf> = None;
    let mut auto_approve = false;
    let mut rest = Vec::new();
    while let Some(arg) = iter.next() {
        if arg == "--project" {
            project = iter.next().map(PathBuf::from);
        } else if let Some(value) = arg.strip_prefix("--project=") {
            project = Some(PathBuf::from(value));
        } else if arg == "--auto-approve" {
            auto_approve = true;
        } else {
            rest.push(arg);
        }
    }

    let mode = match rest.as_slice() {
        [] => StartupMode::Fresh,
        [cmd] if cmd == "resume" => StartupMode::Picker,
        [cmd, id] if cmd == "resume" => StartupMode::Resume(Some(id.clone())),
        [cmd, ..] if cmd == "doctor" => StartupMode::Doctor,
        [cmd, ..] => {
            eprintln!(
                "Unknown command '{}'. Usage:\n  neenee              start a fresh session\n  neenee resume [id]  resume a session (picker when no id)\n  neenee doctor       verify stored session integrity\n\nOptions:\n  --project <path>    operate on the project at <path>\n  --auto-approve      bypass write-tool permission prompts for this session",
                cmd
            );
            std::process::exit(2);
        }
    };
    (mode, project, auto_approve)
}

/// Initialise file-based tracing when `NEENEE_LOG` names a log file.
///
/// A TUI cannot log to stdout (it would corrupt the display), so tracing is
/// opt-in and always writes to a file. Verbosity comes from `RUST_LOG`,
/// defaulting to `info` for the neenee crates. The returned guard flushes the
/// non-blocking writer on drop and must live for the whole process.
pub fn init_tracing() -> Option<WorkerGuard> {
    let path = PathBuf::from(std::env::var_os("NEENEE_LOG")?);
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let file_name = path.file_name()?.to_owned();
    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(dir, file_name));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("neenee=info,neenee_core=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    tracing::info!("neenee tracing initialised");
    Some(guard)
}
