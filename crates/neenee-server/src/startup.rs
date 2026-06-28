//! CLI bootstrap helpers: arg parsing, startup-mode selection, tracing init,
//! and the slash-command vocabulary used to distinguish built-in commands
//! from user-defined ones.
//!
//! These are pure (or near-pure: `init_tracing` does touch the env / filesystem)
//! helpers that lived inline at the top of `main.rs` before being grouped here.
//! They have no dependence on `main.rs` state.

use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;

/// Single source of truth for the built-in slash-command vocabulary.
///
/// Each entry `Variant = "/name" : "description"` generates a [`BuiltinCmd`]
/// enum variant, a row in [`BuiltinCmd::ALL`] (consumed by input completion,
/// `/help`, and the custom-command filter), and an arm of
/// [`BuiltinCmd::from_slash`].
///
/// The dispatch `match` in `main.rs` is over `Option<BuiltinCmd>` and is kept
/// non-exhaustive (no `Some(_)` catch-all). Adding a variant here without a
/// matching handler arm is therefore a **compile error**, so completion,
/// `/help`, and dispatch can never drift — a command appears in all three or
/// the build breaks.
macro_rules! define_builtin_commands {
    ( $( $variant:ident = $name:literal : $desc:literal ),+ $(,)? ) => {
        /// The set of built-in slash commands. Generated from a single
        /// declarative list — see `define_builtin_commands`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum BuiltinCmd {
            $( $variant ),+
        }

        impl BuiltinCmd {
            /// Every built-in command as `(slash_name, description)`, in
            /// declaration order. Completion, `/help`, and the custom-command
            /// filter all read from this — it is the only place command
            /// metadata is written.
            pub const ALL: &[(&'static str, &'static str)] = &[ $( ($name, $desc) ),+ ];

            /// Parse a `/<name>` token into a variant, or `None` when it is
            /// not a built-in (i.e. a custom command). The dispatch `match`
            /// consumes the `None` arm to run the custom-command path.
            pub fn from_slash(input: &str) -> Option<Self> {
                $( if input == $name { return Some(BuiltinCmd::$variant); } )+
                None
            }
        }
    };
}

define_builtin_commands! {
    Provider    = "/provider"     : "Select an LLM provider",
    Tools       = "/tools"        : "Manage session tools (enable/disable)",
    Mcp         = "/mcp"          : "Manage MCP servers (enable/disable, reconnect)",
    Compact     = "/compact"      : "Compact older complete turns now",
    Clear       = "/clear"        : "Clear the conversation history",
    Permissions = "/permissions"  : "Show or clear always-allowed tool rules",
    Config      = "/config"       : "Open user configuration",
    Unattended  = "/unattended"   : "Toggle bypassing write-tool permission prompts (on/off)",
    Review      = "/review"       : "Run an on-demand session-review diagnostic of the current turn",
    Search      = "/search"       : "Semantic search over the project's session history",
    Session     = "/session"      : "Manage durable sessions (status|list|resume|fork|open|new)",
    Sessions    = "/sessions"     : "Browse past sessions",
    Btw         = "/btw"          : "Open a side conversation that runs alongside the main session",
    Resume      = "/resume"       : "Resume the most recent or selected session",
    Pursue      = "/pursue"       : "Pursue a condition: drive the agent until it is met, or manage the pursuit",
    Repeat      = "/repeat"       : "Schedule a prompt on a cron: /repeat <cron> <prompt>",
    Skills      = "/skills"       : "List or reload available skills (list|reload)",
    Skill       = "/skill"        : "Load a skill by name",
    Init        = "/init"         : "Initialize a .neenee/ config tree",
    Export      = "/export"       : "Export this conversation to the clipboard as Markdown",
    Debug       = "/debug"        : "Debug toggles: /debug network on|off",
    Help        = "/help"         : "Show available commands and keybindings",
    Exit        = "/exit"         : "Exit the program",
}

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
    /// Render a single UI component in isolation for interactive development
    /// (`neenee showcase <component>`). No agent, no session, no network —
    /// just the component's model + renderer wired to a real terminal so you
    /// can see and interact with it standalone.
    #[cfg(debug_assertions)]
    Showcase(String),
}

pub fn parse_args(args: Vec<String>) -> (StartupMode, Option<PathBuf>, bool, bool) {
    let mut iter = args.into_iter().peekable();
    let mut project: Option<PathBuf> = None;
    let mut unattended = false;
    let mut single_instance = false;
    let mut rest = Vec::new();
    while let Some(arg) = iter.next() {
        if arg == "--project" {
            project = iter.next().map(PathBuf::from);
        } else if let Some(value) = arg.strip_prefix("--project=") {
            project = Some(PathBuf::from(value));
        } else if arg == "--unattended" {
            unattended = true;
        } else if arg == "--single-instance" {
            single_instance = true;
        } else {
            rest.push(arg);
        }
    }

    let mode = match rest.as_slice() {
        [] => StartupMode::Fresh,
        [cmd] if cmd == "resume" => StartupMode::Picker,
        [cmd, id] if cmd == "resume" => StartupMode::Resume(Some(id.clone())),
        [cmd, ..] if cmd == "doctor" => StartupMode::Doctor,
        #[cfg(debug_assertions)]
        [cmd, component] if cmd == "showcase" => StartupMode::Showcase(component.clone()),
        [cmd, ..] => {
            // `showcase` is a debug-only subcommand; omit it from the release
            // usage string so we don't advertise a command that doesn't exist.
            #[cfg(debug_assertions)]
            let showcase_line =
                "  neenee showcase <name>  render a single UI component standalone\n";
            #[cfg(not(debug_assertions))]
            let showcase_line = "";
            eprintln!(
                "Unknown command '{}'. Usage:\n  neenee                  start a fresh session\n  neenee resume [id]      resume a session (picker when no id)\n  neenee doctor           verify stored session integrity\n{showcase_line}\nOptions:\n  --project <path>        operate on the project at <path>\n  --unattended            bypass write-tool permission prompts for this session\n  --single-instance       require exclusive per-project lock (pre-ADR-0018 default)",
                cmd
            );
            std::process::exit(2);
        }
    };
    (mode, project, unattended, single_instance)
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
