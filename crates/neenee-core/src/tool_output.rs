//! Structured tool output (ADR-0001).
//!
//! Tools historically return `Result<String, String>`, forcing every consumer
//! (the transcript model, the TUI) to recover structure by string-sniffing
//! (`starts_with("Error")`, `"Exit N"`, `"STDERR:"`, …). `ToolOutput` replaces
//! that with a typed result. Migration is incremental via the Strangler
//! pattern: `Tool::call_structured` defaults to delegating to the legacy
//! `Tool::call` and wrapping the text as [`ToolOutput::Text`], so unmigrated
//! tools keep working unchanged while migrated tools override
//! `call_structured` to return richer variants.
//!
//! This module currently declares only the variants the default bridge needs
//! (`Text`, `Error`). Richer variants (`Shell`, `Patch`, `Listing`, `Matches`,
//! …) are added in the step that first migrates a tool to use them, so the
//! type grows with real callers rather than speculatively.

/// Typed result of a tool invocation.
///
/// Neither `PartialEq` nor `Eq` is derived: the [`ToolOutput::Subagent`]
/// variant carries `Vec<Message>` and `Message` does not implement either
/// trait (its `Vec<ImagePart>` base64 payloads make structural equality
/// expensive and uninteresting). Compare via [`ToolOutput::to_text`] or by
/// pattern-matching on the variant in tests.
#[derive(Debug, Clone)]
pub enum ToolOutput {
    /// Plain text or markdown prose. The back-compat variant produced by the
    /// default [`Tool::call_structured`](crate::Tool::call_structured) for any
    /// tool still returning a raw string.
    Text(String),
    /// A structured error. Distinct from [`ToolOutput::Text`] so consumers can
    /// tell a failed call apart from a successful textual result that merely
    /// starts with the literal `"Error"` (which the old string-sniffing
    /// convention could not).
    Error {
        message: String,
        detail: Option<String>,
    },
    /// The user explicitly denied permission for this tool call. Distinct from
    /// [`ToolOutput::Error`] because the action was aborted by the user rather
    /// than failing on its own, and it signals the agent turn to stop.
    PermissionDenied { tool: String },
    /// A shell command execution. Carries stdout/stderr/exit separately so the
    /// UI never has to string-sniff for `Exit N` / `STDOUT:` / `STDERR:`
    /// markers. `truncated` records whether the composed output exceeded the
    /// tool's size cap and was cut.
    Shell {
        command: String,
        stdout: String,
        stderr: String,
        exit: Option<i32>,
        truncated: bool,
    },
    /// Source code / file contents, with an optional language hint (file
    /// extension) so a future renderer can syntax-highlight. `text` is the
    /// (possibly truncation-prefixed) content, identical to what the legacy
    /// string output carried.
    Code { lang: Option<String>, text: String },
    /// A directory / glob listing, as raw entry strings.
    Listing { entries: Vec<String> },
    /// Ripgrep-style search matches, as raw `path:line:content` lines plus the
    /// pattern, so a future renderer can group/highlight without re-parsing.
    Matches { pattern: String, lines: Vec<String> },
    /// A file change. The renderer derives the diff from `old` / `new`
    /// (edit) or from `""` / `new` (create) so the change view comes from
    /// the result payload, not from re-parsing the tool arguments.
    Patch {
        path: String,
        op: PatchOp,
        old: String,
        new: String,
    },
    /// A read-only sub-agent run (produced by the `task` tool). Carries the
    /// sub-agent's full internal transcript so it can be persisted on the
    /// parent session and replayed on resume, plus the actual token usage so
    /// parent-side goal accounting no longer under-counts by 100x. `summary`
    /// is the short text the parent model sees as the tool result.
    Subagent {
        summary: String,
        messages: Vec<crate::Message>,
        usage: crate::TokenUsage,
    },
}

/// Kind of file change in a [`ToolOutput::Patch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOp {
    /// A new file was created (`old` is empty).
    Create,
    /// An existing file was edited.
    Edit,
    /// A file was deleted (`new` is empty).
    Delete,
}

impl ToolOutput {
    /// Wrap a raw string as the back-compat [`ToolOutput::Text`] variant.
    pub fn text(s: impl Into<String>) -> Self {
        ToolOutput::Text(s.into())
    }

    /// Flatten to the legacy display string. This is the bridge that lets the
    /// existing string-based transcript/UI render unchanged while structured
    /// data is also available. `Shell` reproduces the exact format historically
    /// emitted by the bash tool (`tools.rs`), so migrating bash to
    /// [`ToolOutput::Shell`] is invisible to any consumer still reading text.
    pub fn to_text(&self) -> String {
        match self {
            ToolOutput::Text(s) => s.clone(),
            ToolOutput::Error { message, detail } => match detail {
                Some(d) if !d.is_empty() => format!("Error: {}\n{}", message, d),
                _ => format!("Error: {}", message),
            },
            ToolOutput::PermissionDenied { tool } => format!(
                "Permission denied for tool '{}'. Do not retry the same call.",
                tool
            ),
            ToolOutput::Shell {
                command: _,
                stdout,
                stderr,
                exit,
                truncated,
            } => shell_to_text(stdout, stderr, *exit, *truncated),
            ToolOutput::Code { text, .. } => text.clone(),
            ToolOutput::Listing { entries } => entries.join("\n"),
            ToolOutput::Matches { lines, .. } => lines.join("\n"),
            ToolOutput::Patch { path, op, new, .. } => match op {
                PatchOp::Create => format!("Successfully wrote {} bytes to {}", new.len(), path),
                PatchOp::Edit => format!("Edited '{}' successfully", path),
                PatchOp::Delete => format!("Deleted '{}'", path),
            },
            // The parent model sees the sub-agent's textual summary only; the
            // structured transcript travels out-of-band via the parent harness
            // attaching `messages` to the Tool-role message's `children`.
            ToolOutput::Subagent { summary, .. } => summary.clone(),
        }
    }

    /// Whether this output represents a failure. Replaces the TUI's
    /// `output.starts_with("Error")` heuristic with a data-level flag once
    /// tools migrate to emit [`ToolOutput::Error`] / a non-zero [`ToolOutput::Shell`]
    /// exit.
    pub fn is_error(&self) -> bool {
        match self {
            ToolOutput::Error { .. } => true,
            ToolOutput::PermissionDenied { .. } => true,
            ToolOutput::Shell { exit, .. } => !matches!(*exit, Some(0)),
            // Sub-agent failure is signalled by the `summary` starting with
            // `Error:` (mirrors the legacy `Text` convention). We do not add
            // an explicit `error` field because the failure surface is just
            // "the agent didn't produce a useful final answer" — the partial
            // transcript is still valuable and travels alongside.
            ToolOutput::Subagent { summary, .. } => summary.starts_with("Error"),
            ToolOutput::Text(_)
            | ToolOutput::Code { .. }
            | ToolOutput::Listing { .. }
            | ToolOutput::Matches { .. }
            | ToolOutput::Patch { .. } => false,
        }
    }

    /// If this output is a [`ToolOutput::Subagent`], return its nested
    /// transcript and token usage so the harness can attach `children` to the
    /// parent's tool-result message and accumulate real cost into the parent
    /// turn's accounting. Returns `None` for every other variant.
    pub fn subagent_payload(&self) -> Option<(&[crate::Message], crate::TokenUsage)> {
        match self {
            ToolOutput::Subagent {
                messages, usage, ..
            } => Some((messages, *usage)),
            _ => None,
        }
    }
}

impl From<String> for ToolOutput {
    fn from(s: String) -> Self {
        ToolOutput::Text(s)
    }
}

/// An incremental chunk streamed by a long-running tool before its final
/// [`ToolOutput`] lands. Lets the UI render partial output (e.g. a bash
/// command's stdout as it arrives) instead of freezing on a spinner until the
/// process exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStream {
    /// Bytes appended to the running stdout buffer.
    Stdout(String),
    /// Bytes appended to the running stderr buffer.
    Stderr(String),
}

/// Compose the non-truncated bash-tool display string from structured fields
/// (mirrors `BashTool::call`). Pub(crate) so the bash tool can compute the
/// pre-truncation length to decide its `truncated` flag without duplicating
/// the format logic.
pub(crate) fn shell_inner_text(stdout: &str, stderr: &str, exit: Option<i32>) -> String {
    if exit == Some(0) {
        if stdout.is_empty() && !stderr.is_empty() {
            format!("(success, stderr):\n{}", stderr)
        } else {
            stdout.to_string()
        }
    } else {
        format!(
            "Exit {}\nSTDOUT:\n{}\nSTDERR:\n{}",
            exit.unwrap_or(-1),
            stdout,
            stderr
        )
    }
}

/// Reconstruct the legacy bash-tool display string from structured fields.
/// Mirrors `BashTool::call` byte-for-byte so migrating to [`ToolOutput::Shell`]
/// changes nothing for text-based consumers. The truncation policy (8000-char
/// threshold, 4000-char cut) lives here as the back-compat bridge; structured
/// consumers read the raw fields directly and bypass this.
fn shell_to_text(stdout: &str, stderr: &str, exit: Option<i32>, truncated: bool) -> String {
    const MAX_OUTPUT_CHARS: usize = 8000;
    const TRUNCATED_CHARS: usize = 4000;

    let inner = shell_inner_text(stdout, stderr, exit);
    if truncated || inner.len() > MAX_OUTPUT_CHARS {
        format!(
            "[Output truncated: {} chars total]\n{}\n\n[Output was large — use grep or read_file if you need specific parts]",
            inner.len(),
            crate::tools::truncate_utf8(&inner, TRUNCATED_CHARS)
        )
    } else {
        inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips() {
        assert_eq!(ToolOutput::text("hi").to_text(), "hi");
        let v = ToolOutput::from("x".to_string());
        assert!(matches!(v, ToolOutput::Text(s) if s == "x"));
    }

    #[test]
    fn error_to_text_keeps_error_prefix() {
        // The current UI classifies failure by `starts_with("Error")`; the
        // bridge must preserve that until the UI migrates to `is_error()`.
        let e = ToolOutput::Error {
            message: "boom".into(),
            detail: None,
        };
        assert!(e.to_text().starts_with("Error"));
        assert!(e.is_error());
    }

    #[test]
    fn error_with_detail_appends() {
        let e = ToolOutput::Error {
            message: "boom".into(),
            detail: Some("stack\ntrace".into()),
        };
        assert_eq!(e.to_text(), "Error: boom\nstack\ntrace");
    }

    #[test]
    fn shell_success_stdout_only_matches_legacy() {
        let o = ToolOutput::Shell {
            command: "echo hi".into(),
            stdout: "hi\n".into(),
            stderr: "".into(),
            exit: Some(0),
            truncated: false,
        };
        assert_eq!(o.to_text(), "hi\n");
        assert!(!o.is_error());
    }

    #[test]
    fn shell_success_stderr_only_uses_success_stderr_marker() {
        let o = ToolOutput::Shell {
            command: "x".into(),
            stdout: "".into(),
            stderr: "warn".into(),
            exit: Some(0),
            truncated: false,
        };
        assert_eq!(o.to_text(), "(success, stderr):\nwarn");
    }

    #[test]
    fn shell_failure_formats_exit_stdout_stderr() {
        let o = ToolOutput::Shell {
            command: "false".into(),
            stdout: "out".into(),
            stderr: "err".into(),
            exit: Some(1),
            truncated: false,
        };
        assert_eq!(o.to_text(), "Exit 1\nSTDOUT:\nout\nSTDERR:\nerr");
        assert!(o.is_error());
    }

    #[test]
    fn shell_signal_uses_neg1() {
        let o = ToolOutput::Shell {
            command: "x".into(),
            stdout: "".into(),
            stderr: "killed".into(),
            exit: None,
            truncated: false,
        };
        assert_eq!(o.to_text(), "Exit -1\nSTDOUT:\n\nSTDERR:\nkilled");
        assert!(o.is_error());
    }

    #[test]
    fn shell_truncated_wraps_with_markers() {
        let big = "a".repeat(9000);
        let o = ToolOutput::Shell {
            command: "x".into(),
            stdout: big,
            stderr: "".into(),
            exit: Some(0),
            truncated: true,
        };
        let text = o.to_text();
        assert!(text.starts_with("[Output truncated: 9000 chars total]\n"));
        assert!(
            text.ends_with("[Output was large — use grep or read_file if you need specific parts]")
        );
    }

    #[test]
    fn code_to_text_is_the_text() {
        let o = ToolOutput::Code {
            lang: Some("rs".into()),
            text: "fn main() {}".into(),
        };
        assert_eq!(o.to_text(), "fn main() {}");
        assert!(!o.is_error());
    }

    #[test]
    fn listing_to_text_joins_entries() {
        let o = ToolOutput::Listing {
            entries: vec!["src/".into(), "Cargo.toml".into()],
        };
        assert_eq!(o.to_text(), "src/\nCargo.toml");
    }

    #[test]
    fn matches_to_text_joins_lines() {
        let o = ToolOutput::Matches {
            pattern: "foo".into(),
            lines: vec!["a.rs:1:foo".into(), "b.rs:3:foo".into()],
        };
        assert_eq!(o.to_text(), "a.rs:1:foo\nb.rs:3:foo");
    }

    #[test]
    fn subagent_to_text_returns_summary_only() {
        // The parent model only sees the summary; the structured transcript
        // travels out-of-band. This is the contract that lets us persist the
        // sub-agent transcript without polluting the parent's context window.
        let usage = crate::TokenUsage {
            prompt_tokens: 1000,
            completion_tokens: 200,
            total_tokens: 1200,
        };
        let messages = vec![crate::Message::new(crate::Role::Assistant, "internal")];
        let o = ToolOutput::Subagent {
            summary: "external summary".into(),
            messages,
            usage,
        };
        assert_eq!(o.to_text(), "external summary");
        assert!(!o.is_error());
    }

    #[test]
    fn subagent_payload_returns_messages_and_usage() {
        let usage = crate::TokenUsage {
            prompt_tokens: 50,
            completion_tokens: 10,
            total_tokens: 60,
        };
        let messages = vec![
            crate::Message::new(crate::Role::System, "sys"),
            crate::Message::new(crate::Role::Assistant, "answer"),
        ];
        let o = ToolOutput::Subagent {
            summary: "s".into(),
            messages: messages.clone(),
            usage,
        };
        let (got_messages, got_usage) = o.subagent_payload().expect("subagent payload");
        assert_eq!(got_messages.len(), 2);
        assert_eq!(got_usage, usage);
    }

    #[test]
    fn non_subagent_payload_returns_none() {
        let o = ToolOutput::text("plain");
        assert!(o.subagent_payload().is_none());
    }
}
