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
    /// string output carried. `start_line` is the 1-based line number of the
    /// first row of `text` within its source file, so a snippet read with an
    /// `offset` numbers from that line instead of restarting at 1. `0` means
    /// "unknown" and the renderer falls back to 1-based numbering within the
    /// slice — the same sentinel/semantics as [`ToolOutput::Patch::start_line`].
    ///
    /// `prefix` / `suffix` carry **model-facing framing only** (line-range
    /// header, pagination/EOF continuation hints). The renderer ignores them
    /// and draws `text` with the line-number gutter; [`ToolOutput::to_text`]
    /// composes `prefix\n{numbered-text}\nsuffix` for the model, prefixing
    /// each line with its file line number (derived from `start_line`) so the
    /// model can reference exact lines when targeting `offset` or composing
    /// edits. Splitting the two audiences is what lets a paginated read both
    /// render cleanly (pure content, correct line base) and tell the model
    /// exactly where it is, what to target, and how to continue.
    Code {
        lang: Option<String>,
        text: String,
        start_line: usize,
        prefix: Option<String>,
        suffix: Option<String>,
    },
    /// A directory / glob listing, as raw entry strings.
    Listing { entries: Vec<String> },
    /// Ripgrep-style search matches, as raw `path:line:content` lines plus the
    /// pattern, so a future renderer can group/highlight without re-parsing.
    Matches { pattern: String, lines: Vec<String> },
    /// A file change. The renderer derives the diff from `old` / `new`
    /// (edit) or from `""` / `new` (create) so the change view comes from
    /// the result payload, not from re-parsing the tool arguments.
    /// `start_line` is the 1-based file line where `old` begins; `0` means
    /// "unknown" and the renderer falls back to snippet-relative numbering.
    Patch {
        path: String,
        op: PatchOp,
        old: String,
        new: String,
        start_line: usize,
    },
    /// A read-only subagent run (produced by the `task` tool). Carries the
    /// subagent's full internal transcript so it can be persisted on the
    /// parent session and replayed on resume, plus the actual token usage so
    /// parent-side pursuit accounting no longer under-counts by 100x. `summary`
    /// is the short text the parent model sees as the tool result.
    ///
    /// `failed` is the structured failure flag set explicitly by the subagent tool
    /// when the subagent hit a guardrail or errored, replacing the old
    /// `summary.starts_with("Error")` text sniff. The summary text still
    /// carries an `Error:` prefix for the *parent model's* benefit (so it
    /// understands the sub-task did not succeed), but UI classification now
    /// reads this field instead of pattern-matching the prose.
    Subagent {
        summary: String,
        messages: Vec<crate::Message>,
        usage: crate::TokenUsage,
        failed: bool,
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

/// Prefix each line of `text` with its 1-based file line number, derived from
/// `start_line`. This is what the model sees in tool results — the line
/// numbers let it reference exact lines when targeting `offset` in a
/// follow-up read or composing an edit. `start_line == 0` falls back to
/// 1-based numbering within the slice.
fn number_code_lines(text: &str, start_line: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let base = if start_line == 0 { 1 } else { start_line };
    text.lines()
        .enumerate()
        .map(|(i, line)| format!("{}: {}", base + i, line))
        .collect::<Vec<_>>()
        .join("\n")
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
            ToolOutput::Code {
                text,
                prefix,
                suffix,
                start_line,
                ..
            } => {
                let numbered = number_code_lines(text, *start_line);
                match (prefix, suffix) {
                    (Some(pre), Some(suf)) => format!("{}\n{}\n{}", pre, numbered, suf),
                    (Some(pre), None) => format!("{}\n{}", pre, numbered),
                    (None, Some(suf)) => format!("{}\n{}", numbered, suf),
                    (None, None) => numbered,
                }
            }
            ToolOutput::Listing { entries } => entries.join("\n"),
            ToolOutput::Matches { lines, .. } => lines.join("\n"),
            ToolOutput::Patch { path, op, new, .. } => match op {
                PatchOp::Create => format!("Successfully wrote {} bytes to {}", new.len(), path),
                PatchOp::Edit => format!("Edited '{}' successfully", path),
                PatchOp::Delete => format!("Deleted '{}'", path),
            },
            // The parent model sees the subagent's textual summary only; the
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
            ToolOutput::Subagent { failed, .. } => *failed,
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
pub fn shell_inner_text(stdout: &str, stderr: &str, exit: Option<i32>) -> String {
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
            truncate_utf8(&inner, TRUNCATED_CHARS)
        )
    } else {
        inner
    }
}

/// Truncate `text` to at most `max_bytes` without splitting a multibyte UTF-8
/// character. Returns a `&str` slice of `text`.
///
/// Shared by the structured-output formatter (in this crate) and the tool
/// implementations (`neenee-tools`) that produce the outputs being formatted.
pub fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
            start_line: 1,
            prefix: None,
            suffix: None,
        };
        assert_eq!(o.to_text(), "1: fn main() {}");
        assert!(!o.is_error());
    }

    #[test]
    fn code_start_line_round_trips_and_defaults_to_zero() {
        // `start_line` drives per-line numbering in `to_text()` so the model
        // can reference exact file lines. It must survive cloning so an offset
        // snippet keeps its line base.
        let o = ToolOutput::Code {
            lang: None,
            text: "x".into(),
            start_line: 42,
            prefix: None,
            suffix: None,
        };
        assert_eq!(o.to_text(), "42: x");
        let cloned = o.clone();
        match cloned {
            ToolOutput::Code { start_line, .. } => assert_eq!(start_line, 42),
            _ => unreachable!(),
        }
    }

    #[test]
    fn code_prefix_suffix_frame_the_content_for_the_model() {
        // The renderer draws `text`; the model sees framing composed around
        // line-numbered content. This split is what makes pagination loop-safe:
        // the model gets a concrete continuation without polluting the rendered
        // code block.
        let with_both = ToolOutput::Code {
            lang: None,
            text: "body".into(),
            start_line: 100,
            prefix: Some("[f: lines 100-100 of 5000]".into()),
            suffix: Some("[4900 more lines — read with offset=101]".into()),
        };
        assert_eq!(
            with_both.to_text(),
            "[f: lines 100-100 of 5000]\n100: body\n[4900 more lines — read with offset=101]"
        );

        let prefix_only = ToolOutput::Code {
            lang: None,
            text: "body".into(),
            start_line: 100,
            prefix: Some("[f: lines 100-105 of 105]".into()),
            suffix: None,
        };
        assert_eq!(
            prefix_only.to_text(),
            "[f: lines 100-105 of 105]\n100: body"
        );
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
        // subagent transcript without polluting the parent's context window.
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
            failed: false,
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
            failed: false,
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

    #[test]
    fn subagent_failed_flag_drives_is_error_not_summary_text() {
        // Regression for the text-sniff removal: a subagent whose summary
        // starts with "Error" but carries `failed: false` must NOT classify
        // as an error, and vice versa.
        let with_flag = ToolOutput::Subagent {
            summary: "partial findings".into(),
            messages: Vec::new(),
            usage: crate::TokenUsage::default(),
            failed: true,
        };
        assert!(with_flag.is_error());

        let no_flag = ToolOutput::Subagent {
            summary: "Error: legacy text".into(),
            messages: Vec::new(),
            usage: crate::TokenUsage::default(),
            failed: false,
        };
        assert!(!no_flag.is_error());
    }
}
