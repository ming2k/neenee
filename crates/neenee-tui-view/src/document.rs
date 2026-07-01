//! Semantic document model for the TUI.
//!
//! Unlike storing raw strings, this model preserves the structure of messages
//! so that selection and copy operate on semantic units (blocks) rather than
//! terminal grid characters.

use neenee_core::{EnvoyEvent, Role};

/// Lifecycle of a tool step, stored explicitly (not inferred from `output`)
/// so an aborted call has its own terminal state instead of being stuck in
/// "no output yet". This is the single source of truth for tool-step state —
/// the renderer classifies it into a [`crate::render::tools::ToolStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolStepStatus {
    /// Still in flight (no terminal event observed yet).
    #[default]
    Running,
    /// Finished with a non-error output.
    Ok,
    /// Finished with an explicit error output.
    Failed,
    /// Aborted because the user denied permission for the call.
    Denied,
    /// Aborted mid-flight (e.g. the user interrupted the turn). Terminal, just
    /// like `Ok`/`Failed`: a later result or cancel event is ignored.
    Cancelled,
}

impl ToolStepStatus {
    /// Whether this state can still transition (i.e. the step is in flight).
    pub fn is_running(self) -> bool {
        matches!(self, ToolStepStatus::Running)
    }
}

#[derive(Debug, Clone)]
pub enum MessageKind {
    Text,
    ToolStep {
        id: String,
        name: String,
        /// The bound envoy profile name (`explore` / `plan` / `verify` / …)
        /// for an envoy-spawning tool step, populated from the first
        /// `EnvoyEvent::Started` and used to label the step by its role.
        /// `None` for non-envoy steps, or until the `Started` event lands.
        profile: Option<String>,
        arguments: String,
        output: Option<String>,
        /// Typed result (ADR-0001). `None` until the result lands, then a
        /// [`neenee_core::ToolOutput`] carrying structured data (e.g. a shell
        /// exit code) alongside the legacy `output` text. Consumed by the
        /// renderer for data-level classification — `finish_tool_step` derives
        /// [`ToolStepStatus`] from `ToolOutput::is_error()` instead of
        /// string-sniffing the output, and `bash_command_for` reads the typed
        /// `Shell` command. The legacy `output`/`arguments` strings remain the
        /// fallback for restored sessions that predate the typed payload.
        ///
        /// Boxed to keep this enum variant small: `ToolOutput` (and especially
        /// its `Envoy`/`Patch` variants) is large enough that an unboxed
        /// `Option<ToolOutput>` would dominate the `MessageKind` enum size
        /// (clippy::large_enum_variant). The indirection is transparent to
        /// callers — the surrounding accessors deref it as needed.
        structured: Option<Box<neenee_core::ToolOutput>>,
        /// Explicit lifecycle. Kept in sync with `output` by the
        /// `finish_tool_step` / `cancel_tool_step` transitions below.
        status: ToolStepStatus,
        expanded: bool,
        /// Whether the user has manually pinned `expanded`. While true, the
        /// auto/system setter (`set_tool_step_expanded`) is a no-op so
        /// lifecycle transitions can't override a deliberate user choice.
        user_pinned: bool,
        duration_ms: Option<u64>,
        /// Wall-clock instant the step started, so the UI can show a live
        /// elapsed time while the call (or envoy) is still running.
        /// `Instant` is cheap to capture at construction time and is not
        /// serialized — session restore reconstructs finished steps without it.
        started_at: Option<std::time::Instant>,
        /// Child events emitted by an envoy spawned from this tool step.
        children: Vec<TranscriptMessage>,
    },
    Thinking {
        content: String,
        duration_ms: Option<u64>,
        expanded: bool,
        /// User-pinned flag — see [`MessageKind::ToolStep::user_pinned`].
        user_pinned: bool,
    },
    /// A harness-level notice — errors, turn-pause signals, compaction
    /// summaries, provider switches, and other status lines that previously
    /// were smuggled through `Role::System` with hand-rolled `"Error: "`
    /// / `"System: "` text prefixes. Carrying an explicit [`NoticeSeverity`]
    /// lets one renderer (the `render::notice` module) own the
    /// severity→color/icon mapping and lets callers stop string-sniffing.
    Notice {
        severity: NoticeSeverity,
    },
}

/// Severity of a [`MessageKind::Notice`]. Drives the color and the leading
/// icon through the central severity→presentation map in
/// `render/notice.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    /// Neutral status (compaction summary, provider switch, …). Replaces the
    /// old `Role::System` + `system_text()` rendering.
    Info,
    /// A non-terminal condition that needs attention.
    Warning,
    /// A terminal failure surfaced from the harness or a tool.
    Error,
}

pub fn notice_severity_from_core(severity: neenee_core::NoticeSeverity) -> NoticeSeverity {
    match severity {
        neenee_core::NoticeSeverity::Info => NoticeSeverity::Info,
        neenee_core::NoticeSeverity::Warning => NoticeSeverity::Warning,
        neenee_core::NoticeSeverity::Error => NoticeSeverity::Error,
    }
}

/// Table column text alignment for GFM tables parsed by the in-house parser,
/// kept as a separate type so the `Block` definition stays dependency-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlignment {
    None,
    Left,
    Center,
    Right,
}

/// A byte range `[start, end)` within a prose block's `content` that should be
/// rendered as inline code. The in-house parser keeps the backtick delimiters
/// in the flattened `content` and records the range here so the renderer can
/// paint it on the code surface without disturbing the byte-addressable
/// copy/selection model (which still sees plain text).
///
/// Ranges always cover the full `` `…` `` span including both backticks, and
/// are clamped to `content.len()`. An empty vector means "no inline code".
pub type CodeRange = (usize, usize);

/// A single semantic block within a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Plain text paragraph.
    Text {
        content: String,
        /// Byte ranges of inline-code runs within `content` (see [`CodeRange`]).
        code_ranges: Vec<CodeRange>,
        /// Byte ranges of strong/bold text runs within `content`.
        bold_ranges: Vec<CodeRange>,
    },
    /// Inline or fenced code.
    Code {
        language: Option<String>,
        content: String,
    },
    /// A heading.
    Heading {
        level: u8,
        content: String,
        /// Byte ranges of inline-code runs within `content`.
        code_ranges: Vec<CodeRange>,
        /// Byte ranges of strong/bold text runs within `content`.
        bold_ranges: Vec<CodeRange>,
    },
    /// A list item, preserving its marker and nesting level.
    ListItem {
        content: String,
        /// Byte ranges of inline-code runs within `content`.
        code_ranges: Vec<CodeRange>,
        /// Byte ranges of strong/bold text runs within `content`.
        bold_ranges: Vec<CodeRange>,
        ordered: Option<u64>,
        depth: usize,
        checked: Option<bool>,
    },
    /// A blockquote.
    Quote {
        content: String,
        /// Byte ranges of inline-code runs within `content`.
        code_ranges: Vec<CodeRange>,
        /// Byte ranges of strong/bold text runs within `content`.
        bold_ranges: Vec<CodeRange>,
    },
    /// A GFM-style table, kept as a semantic unit so columns stay aligned and
    /// copy yields the rendered grid rather than re-wrapped prose.
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        aligns: Vec<TableAlignment>,
        /// Pre-rendered aligned grid (what is drawn and what copy returns).
        rendered: String,
    },
    /// A horizontal rule.
    Rule,
    /// Soft / hard line break marker.
    Break,
}

impl Block {
    /// Returns the raw text content of this block (without formatting).
    pub fn raw_text(&self) -> &str {
        match self {
            Block::Text { content, .. } => content,
            Block::Code { content, .. } => content,
            Block::Heading { content, .. } => content,
            Block::ListItem { content, .. } => content,
            Block::Quote { content, .. } => content,
            Block::Table { rendered, .. } => rendered,
            Block::Rule => "",
            Block::Break => "\n",
        }
    }

    /// Returns true if this block is empty.
    pub fn is_empty(&self) -> bool {
        self.raw_text().is_empty()
    }
}

/// Total character count of the context a message stream occupies, used by the
/// header's context-window indicator. `raw` is display state for tool steps
/// (a short summary when collapsed), so tool-step bulk is measured directly
/// from `arguments` + `output` + nested children; thinking text uses `content`;
/// plain-text messages use `raw`.
fn context_token_weight(messages: &[TranscriptMessage]) -> i64 {
    let mut tokens: i64 = 0;
    for m in messages {
        match &m.kind {
            MessageKind::Text => tokens += neenee_core::count_tokens(&m.raw),
            MessageKind::Notice { .. } => tokens += neenee_core::count_tokens(&m.raw),
            MessageKind::Thinking { content, .. } => tokens += neenee_core::count_tokens(content),
            MessageKind::ToolStep {
                arguments,
                output,
                children,
                ..
            } => {
                tokens += neenee_core::count_tokens(arguments);
                if let Some(o) = output {
                    tokens += neenee_core::count_tokens(o);
                }
                tokens += context_token_weight(children);
            }
        }
    }
    tokens
}

/// Token estimate for the active context, using `neenee_core`'s char-class
/// estimator ([`neenee_core::count_tokens`]). This accounts for CJK glyphs,
/// code punctuation, and other Unicode — so the on-screen indicator tracks
/// reality for mixed Chinese + code conversations instead of the old flat
/// `bytes / 4` heuristic.
///
/// Note: this counts the *displayed* transcript, which includes `Thinking`
/// (reasoning) content. The runtime decision layer (`estimate_tokens`)
/// excludes reasoning because it is never sent to providers. The display
/// figure is therefore an intentional upper bound, not a bug.
pub fn estimate_context_tokens(messages: &[TranscriptMessage]) -> usize {
    context_token_weight(messages).max(1) as usize
}

/// Lifecycle of a user-authored message from the user's point of view.
///
/// All other roles are inherently "delivered" (the harness only renders them
/// once they exist), so this only matters on `Role::User` messages. The TUI
/// uses it to draw a distinct "⏸ Queued" panel while a message is waiting for
/// the in-flight turn to finish, and the event loop flips it back to
/// [`DeliveryStatus::Delivered`] once the queued message is actually shipped
/// to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeliveryStatus {
    /// The message has been handed off to the agent (or is an assistant /
    /// tool / system message that doesn't go through the queue).
    #[default]
    Delivered,
    /// The user pressed Enter while a turn was still running, so the message
    /// is staged in the TUI's send queue and will be dispatched automatically
    /// when the harness returns to idle.
    Queued,
}

/// A structured transcript message.
/// What kind of user turn this `Role::User` message originates from. Only
/// meaningful for user messages; the other roles carry the default
/// ([`UserMessageOrigin::Chat`]) and it is never consulted for them.
///
/// The Activity modal uses this to decide whether a `Role::User` message is
/// the genuine prompt that drove the current turn: slash commands
/// (`/review …`) and shell passthroughs (`!ls`) are surfaced as user messages
/// in the transcript but are *not* the LLM prompt, so they must not be shown
/// as the turn's "Prompt".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserMessageOrigin {
    /// A normal chat prompt the user composed and sent to the model. This is
    /// the only origin the Activity modal treats as the turn's prompt.
    #[default]
    Chat,
    /// A slash command (`/review`, `/pursue …`, …). The harness handles these
    /// directly; the model never sees them as a prompt.
    Slash,
    /// A `!command` shell passthrough run directly through the bash tool,
    /// bypassing the model entirely.
    Shell,
}

/// Monotonic source of per-message identities. A message keeps its `id` across
/// the per-frame clone into `App::messages`, so the renderer
/// can use it as a stable cache key for the message's laid-out height (see the
/// height cache in `render`). Ids are process-unique; cloning a message copies
/// its id (a clone represents the same logical message), which is exactly what
/// the height cache wants.
static NEXT_MESSAGE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_message_id() -> u64 {
    NEXT_MESSAGE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub struct TranscriptMessage {
    /// Stable, process-unique identity used as the renderer's height-cache key.
    /// Assigned at construction and preserved across clones.
    pub id: u64,
    pub role: Role,
    pub blocks: Vec<Block>,
    /// The original raw markdown/text, preserved for exact copy.
    pub raw: String,
    pub kind: MessageKind,
    /// What kind of user turn this `Role::User` message is. Defaults to
    /// [`UserMessageOrigin::Chat`]; slash commands and shell passthroughs mark
    /// themselves so they are not mistaken for the turn's driving prompt.
    pub origin: UserMessageOrigin,
    /// Lifecycle of this message from the send queue's point of view. Only
    /// `Role::User` messages ever carry [`DeliveryStatus::Queued`]; everything
    /// else stays at the default [`DeliveryStatus::Delivered`]. The renderer
    /// and the queue dispatch/recall paths key off this.
    pub delivery: DeliveryStatus,
    /// Provider/solution id that produced this message, mirrored from the
    /// core [`neenee_core::Message`] so the transcript stays traceable across
    /// model switches. `None` for messages that don't carry attribution.
    pub provider: Option<String>,
    /// Model id that produced this message, companion to [`TranscriptMessage::provider`].
    pub model: Option<String>,
    /// The tool-round this assistant-side message belongs to (1-indexed,
    /// stamped from the harness's `TurnStarted` counter). Only tool steps
    /// carry it in practice. The renderer uses it to insert a round-boundary
    /// separator between adjacent collapsed tool steps that belong to
    /// different rounds, so two tool-only rounds never read as one batch.
    /// `None` (the default, and for restored sessions that predate the stamp)
    /// means "round unknown" — the renderer then preserves the legacy
    /// same-round flush stack.
    pub turn: Option<u64>,
}

impl TranscriptMessage {
    pub fn new(role: Role, raw: impl Into<String>) -> Self {
        let raw = sanitize_text(&raw.into()).into_owned();
        // User messages are rendered verbatim as plain text — no markdown
        // interpretation — so pasted text containing markdown-like syntax
        // does not get mangled into headings/code fences/lists and the
        // transcript stays readable. The raw text becomes a single `Text`
        // block; `wrap_text` preserves intra-block line breaks.
        let blocks = if role == Role::User {
            parse_blocks_plain(&raw)
        } else {
            parse_blocks(&raw)
        };
        Self {
            id: next_message_id(),
            role,
            blocks,
            raw,
            kind: MessageKind::Text,
            delivery: DeliveryStatus::default(),
            origin: UserMessageOrigin::Chat,
            provider: None,
            model: None,
            turn: None,
        }
    }

    /// Label this `Role::User` message with its turn origin (slash command /
    /// shell passthrough). No-op for non-user messages, which never surface
    /// an origin. Builder-style, used alongside [`Self::queued`].
    pub fn with_origin(mut self, origin: UserMessageOrigin) -> Self {
        self.origin = origin;
        self
    }

    /// Mark this message as queued in the send queue (waiting for the
    /// in-flight turn to finish before it is dispatched). Only meaningful on
    /// `Role::User` messages; the renderer and dispatch logic key off this.
    pub fn queued(mut self) -> Self {
        self.delivery = DeliveryStatus::Queued;
        self
    }

    /// Stamp the provider/solution id and model that produced this message.
    pub fn with_attribution(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.provider = Some(provider.into());
        self.model = Some(model.into());
        self
    }

    /// Stamp the tool round this message belongs to (see [`TranscriptMessage::turn`]).
    pub fn with_turn(mut self, round: u64) -> Self {
        self.turn = Some(round);
        self
    }

    /// The `(provider, model)` pair to show as an attribution badge, when this
    /// message carries at least a model. Used by the renderer to label which
    /// model produced a turn; `None` when the message has no attribution
    /// (user/system messages, or untagged history).
    pub fn attribution_label(&self) -> Option<(String, String)> {
        let model = self.model.clone()?;
        let provider = self.provider.clone().unwrap_or_default();
        Some((provider, model))
    }

    pub fn tool_step(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        let mut message = Self {
            id: next_message_id(),
            role: Role::Tool,
            blocks: Vec::new(),
            raw: String::new(),
            kind: MessageKind::ToolStep {
                id: id.into(),
                name: name.into(),
                profile: None,
                arguments: arguments.into(),
                output: None,
                structured: None,
                status: ToolStepStatus::Running,
                expanded: false,
                user_pinned: false,
                duration_ms: None,
                started_at: Some(std::time::Instant::now()),
                children: Vec::new(),
            },
            delivery: DeliveryStatus::default(),
            origin: UserMessageOrigin::Chat,
            provider: None,
            model: None,
            turn: None,
        };
        message.refresh_tool_step();
        message
    }

    pub fn finish_tool_step(
        &mut self,
        id: &str,
        output: impl Into<String>,
        structured: neenee_core::ToolOutput,
        duration_ms: u64,
    ) -> bool {
        let MessageKind::ToolStep {
            id: step_id,
            output: step_output,
            structured: step_structured,
            status,
            duration_ms: step_duration,
            ..
        } = &mut self.kind
        else {
            return false;
        };
        if step_id != id || !status.is_running() {
            return false;
        }
        let output = output.into();
        // Classify from the structured result (data-level: a non-zero shell
        // exit, an explicit `ToolOutput::Error`, a `failed` envoy). The
        // legacy `starts_with("Error")` text fallback was removed once tool
        // error sites migrated to `ToolOutput::Error` and envoys carried
        // an explicit `failed` flag — classification is now fully data-driven.
        // Permission denial gets its own status so the UI shows it distinctly
        // from a runtime error.
        *status = if matches!(structured, neenee_core::ToolOutput::PermissionDenied { .. }) {
            ToolStepStatus::Denied
        } else if structured.is_error() {
            ToolStepStatus::Failed
        } else {
            ToolStepStatus::Ok
        };
        *step_output = Some(output);
        *step_structured = Some(Box::new(structured));
        *step_duration = Some(duration_ms);
        self.refresh_tool_step();
        true
    }

    /// Accumulate an incremental stream chunk into a still-running tool step,
    /// so the UI can render partial output (e.g. bash stdout) live. The first
    /// chunk initializes a partial [`neenee_core::ToolOutput::Shell`]; the
    /// terminal `finish_tool_step` later overwrites it with the final result.
    /// Returns `false` if this isn't a matching running step.
    pub fn push_tool_stream(&mut self, id: &str, stream: &neenee_core::ToolStream) -> bool {
        let MessageKind::ToolStep {
            id: step_id,
            structured,
            status,
            ..
        } = &mut self.kind
        else {
            return false;
        };
        if step_id != id || !status.is_running() {
            return false;
        }
        if !matches!(
            structured.as_deref(),
            Some(neenee_core::ToolOutput::Shell { .. })
        ) {
            *structured = Some(Box::new(neenee_core::ToolOutput::Shell {
                command: String::new(),
                stdout: String::new(),
                stderr: String::new(),
                lines: Vec::new(),
                exit: None,
                truncated: false,
                // Still-streaming seed: the real termination lands with the
                // final result (`finish_tool_step`). Default until then.
                termination: neenee_core::tool_output::ShellTermination::default(),
            }));
        }
        if let Some(neenee_core::ToolOutput::Shell {
            stdout,
            stderr,
            lines,
            ..
        }) = structured.as_deref_mut()
        {
            // Build the TUI-authoritative `lines` view alongside the flat
            // strings so the streaming view matches the final result: stderr
            // stays red-tinted and stdout/stderr keep their true arrival
            // interleaving, instead of the all-stdout-then-all-stderr
            // degraded band the empty-`lines` fallback used to force.
            //
            // Each stream chunk is one complete `\n`-terminated line (bash's
            // capture is line-buffered and emits `format!("{text}\n")`), so
            // split on `\n` and tag each non-empty piece with its source
            // stream. Trailing empties (from the terminal `\n`) are dropped so
            // they don't paint phantom blank rows.
            let stream_tag = match stream {
                neenee_core::ToolStream::Stdout(_) => neenee_core::tool_output::ShellStream::Out,
                neenee_core::ToolStream::Stderr(_) => neenee_core::tool_output::ShellStream::Err,
            };
            let text = match stream {
                neenee_core::ToolStream::Stdout(s) | neenee_core::ToolStream::Stderr(s) => s,
            };
            for piece in text.split('\n') {
                if !piece.is_empty() {
                    lines.push(neenee_core::tool_output::ShellLine {
                        stream: stream_tag,
                        text: piece.to_string(),
                    });
                }
            }
            match stream {
                neenee_core::ToolStream::Stdout(s) => stdout.push_str(s),
                neenee_core::ToolStream::Stderr(s) => stderr.push_str(s),
            }
        }
        self.refresh_tool_step();
        true
    }

    /// Mark a still-running tool step as cancelled. Idempotent: a step that
    /// already reached a terminal state (`Ok` / `Failed` / `Cancelled`) is left
    /// untouched and returns `false`. When the step is a `task` (envoy),
    /// its still-running nested tool children are cancelled too, so an aborted
    /// envoy never leaves a "running" child step behind.
    pub fn cancel_tool_step(&mut self, id: &str) -> bool {
        let MessageKind::ToolStep {
            id: step_id,
            status,
            ..
        } = &mut self.kind
        else {
            return false;
        };
        if step_id != id || !status.is_running() {
            return false;
        }
        // Apply the transition through `cancel_all_running`, which also handles
        // the nested-children sweep and refreshes the rendered view in one
        // place.
        self.cancel_all_running()
    }

    /// Recursively cancel every still-running tool step within this message
    /// (used for envoy children and as a defensive sweep). Returns `true`
    /// if anything transitioned.
    pub fn cancel_all_running(&mut self) -> bool {
        let (step_running, child_changed) = {
            let MessageKind::ToolStep {
                status,
                started_at,
                duration_ms,
                children,
                ..
            } = &mut self.kind
            else {
                return false;
            };
            let mut changed = false;
            if status.is_running() {
                *status = ToolStepStatus::Cancelled;
                // Freeze the elapsed time at the moment of cancellation so the
                // step stops showing a live-running timer.
                if duration_ms.is_none() {
                    *duration_ms = started_at
                        .map(|started| started.elapsed().as_millis() as u64)
                        .or(Some(0));
                }
                changed = true;
            }
            let mut child_changed = changed;
            for child in children.iter_mut() {
                child_changed |= child.cancel_all_running();
            }
            (changed, child_changed)
        };
        if step_running || child_changed {
            self.refresh_tool_step();
        }
        step_running || child_changed
    }

    /// The explicit lifecycle of a tool step, or `None` for non-tool messages.
    pub fn tool_step_status(&self) -> Option<ToolStepStatus> {
        match &self.kind {
            MessageKind::ToolStep { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Append an envoy event as a nested child of this tool step.
    ///
    /// Returns `true` if this message is a tool step and the event was stored.
    pub fn push_envoy_event(&mut self, event: &EnvoyEvent) -> bool {
        let MessageKind::ToolStep {
            children, profile, ..
        } = &mut self.kind
        else {
            return false;
        };
        match event {
            // The envoy announced its role — stamp it on the step so the
            // label can render "explore: …" / "plan: …" instead of a generic
            // "Envoy". No child message is produced.
            EnvoyEvent::Started { profile: name } => {
                *profile = Some(name.clone());
            }
            EnvoyEvent::StreamStart => {
                children.push(TranscriptMessage::new(Role::Assistant, ""));
            }
            EnvoyEvent::StreamDelta(delta) => {
                if let Some(last) = children
                    .last_mut()
                    .filter(|m| m.role == Role::Assistant && matches!(m.kind, MessageKind::Text))
                {
                    last.push_stream(&sanitize_text(delta));
                } else {
                    let mut msg = TranscriptMessage::new(Role::Assistant, "");
                    msg.push_stream(&sanitize_text(delta));
                    children.push(msg);
                }
            }
            EnvoyEvent::StreamEnd(content) => {
                if let Some(last) = children.last_mut().filter(|m| m.role == Role::Assistant) {
                    last.raw = content.clone();
                    last.reparse();
                } else {
                    children.push(TranscriptMessage::new(Role::Assistant, content.clone()));
                }
            }
            EnvoyEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                children.push(TranscriptMessage::tool_step(
                    id.clone(),
                    name.clone(),
                    arguments.clone(),
                ));
            }
            EnvoyEvent::ToolResult {
                id,
                output,
                duration_ms,
                ..
            } => {
                if let Some(child) = children.iter_mut().find(|m| {
                    m.is_tool_step()
                        && if let MessageKind::ToolStep {
                            id: step_id,
                            output: None,
                            ..
                        } = &m.kind
                        {
                            step_id == id
                        } else {
                            false
                        }
                }) {
                    child.finish_tool_step(
                        id,
                        output.clone(),
                        neenee_core::ToolOutput::text(output.clone()),
                        *duration_ms,
                    );
                } else {
                    let mut msg = TranscriptMessage::tool_step(id.clone(), "tool", "{}");
                    msg.finish_tool_step(
                        id,
                        output.clone(),
                        neenee_core::ToolOutput::text(output.clone()),
                        *duration_ms,
                    );
                    children.push(msg);
                }
            }
            EnvoyEvent::Notice(notice) => {
                children.push(TranscriptMessage::notice(
                    notice_severity_from_core(notice.severity),
                    notice.render_text(),
                ));
            }
            EnvoyEvent::Activity(_) => {}
            // Full-duplex (ADR-0029): an envoy surfaced a permission /
            // ask_user request up through the envoy tool. The down-direction
            // reply (registry → handle → reply_permission / reply_user_question)
            // is wired at the agent layer; rendering the nested prompt in the
            // TUI and routing the user's answer back down is the harness↔TUI
            // integration step that follows. Until then these are observed but
            // not rendered as a nested child step (the request still reaches
            // the harness via the `RoundEvent::Envoy` envelope, so a future
            // handler can attach without changing the event shape).
            EnvoyEvent::PermissionRequest(_)
            | EnvoyEvent::UserQuestionRequest(_)
            | EnvoyEvent::InputRequest(_) => {}
        }
        true
    }

    pub fn is_tool_step(&self) -> bool {
        matches!(self.kind, MessageKind::ToolStep { .. })
    }

    pub fn tool_step_expanded(&self) -> Option<bool> {
        match &self.kind {
            MessageKind::ToolStep { expanded, .. } => Some(*expanded),
            _ => None,
        }
    }

    /// Auto/system disclosure setter: sets `expanded` **unless** the user has
    /// pinned the step (in which case it's a no-op). This is what lifecycle
    /// transitions (start / finish / cancel) and step creation call, so the
    /// derived default never fights a manual choice. User-driven toggles go
    /// through [`Self::pin_tool_step_expanded`].
    pub fn set_tool_step_expanded(&mut self, expanded: bool) {
        if let MessageKind::ToolStep {
            expanded: current,
            user_pinned,
            ..
        } = &mut self.kind
        {
            if *user_pinned {
                return;
            }
            *current = expanded;
            self.refresh_tool_step();
        }
    }

    /// User-driven disclosure change: force `expanded` and mark it pinned so
    /// later lifecycle transitions leave it alone.
    pub fn pin_tool_step_expanded(&mut self, expanded: bool) {
        if let MessageKind::ToolStep {
            expanded: current,
            user_pinned,
            ..
        } = &mut self.kind
        {
            *current = expanded;
            *user_pinned = true;
            self.refresh_tool_step();
        }
    }

    /// The `envoy` tool spawns an envoy. Such tool steps are rendered as a
    /// compact, non-expandable step that navigates into a dedicated envoy
    /// view on activation (see the TUI focus stack) rather than expanding
    /// inline.
    pub fn is_envoy_task(&self) -> bool {
        matches!(&self.kind, MessageKind::ToolStep { name, .. } if name == "envoy")
    }

    /// The call id of a tool step, used as the addressable identity of a
    /// envoy task for the focus stack.
    pub fn tool_step_call_id(&self) -> Option<&str> {
        match &self.kind {
            MessageKind::ToolStep { id, .. } => Some(id),
            _ => None,
        }
    }

    /// The nested child messages emitted by an envoy task. Returns `None`
    /// for non-tool-step messages.
    pub fn envoy_children(&self) -> Option<&[TranscriptMessage]> {
        match &self.kind {
            MessageKind::ToolStep { children, .. } => Some(children),
            _ => None,
        }
    }

    /// Mutable access to a tool step's child messages (used when the view is
    /// zoomed into an envoy and its children are the active message stream).
    pub fn envoy_children_mut(&mut self) -> Option<&mut Vec<TranscriptMessage>> {
        match &mut self.kind {
            MessageKind::ToolStep { children, .. } => Some(children),
            _ => None,
        }
    }

    /// Short label for the envoy, shown in the envoy view's navigation
    /// bar. Prefixed with the role (`explore` / `plan` / `verify` / …) when
    /// the `Started` event has identified it, so the bar reads e.g.
    /// `plan · write the implementation plan` rather than a bare description.
    pub fn envoy_label(&self) -> String {
        let MessageKind::ToolStep {
            arguments, profile, ..
        } = &self.kind
        else {
            return "Envoy".to_string();
        };
        let label = parse_arguments_kv(arguments)
            .into_iter()
            .find(|(k, _)| k == "description")
            .map(|(_, v)| v)
            .unwrap_or_else(|| "Envoy".to_string());
        let label = truncate(&label, 48);
        match profile {
            Some(role) => format!("{} · {}", role, label),
            None => label,
        }
    }

    /// One-line live status derived from the envoy's children and the
    /// parent tool step's completion state, e.g. `↳ Running · 3 tool calls ·
    /// Grep "foo"` or `↳ Completed · 3 tool calls · 1.2s`. Returns
    /// `None` for non-task steps. Duration is only shown once the step reaches
    /// a terminal state; a running step surfaces progress instead of an
    /// accumulating timer.
    pub fn envoy_status_line(&self) -> Option<String> {
        if !self.is_envoy_task() {
            return None;
        }
        let MessageKind::ToolStep {
            status,
            duration_ms,
            children,
            ..
        } = &self.kind
        else {
            return None;
        };
        let tool_calls = children.iter().filter(|child| child.is_tool_step()).count();
        let line = match status {
            ToolStepStatus::Failed => format!("↳ Failed · {} tool calls", tool_calls),
            ToolStepStatus::Denied => format!("↳ Denied · {} tool calls", tool_calls),
            ToolStepStatus::Cancelled => format!("↳ Cancelled · {} tool calls", tool_calls),
            ToolStepStatus::Ok => format!(
                "↳ Completed · {} tool calls · {}",
                tool_calls,
                duration_text(*duration_ms)
            ),
            ToolStepStatus::Running => {
                // Running: show accumulated tool-call count followed by the
                // most recent child activity. The elapsed timer is deliberately
                // omitted while the step is in flight; duration is surfaced only
                // once the step reaches a terminal state.
                let stats = format!("· {} tool calls", tool_calls);
                match children.last() {
                    Some(child)
                        if child.is_tool_step()
                            && child.tool_step_status() == Some(ToolStepStatus::Running) =>
                    {
                        // A tool step still in flight.
                        let header = child
                            .tool_step_summary()
                            .unwrap_or_else(|| "tool".to_string());
                        format!("↳ Running {} · {}", stats, header)
                    }
                    Some(child) if child.role == Role::Assistant && !child.raw.is_empty() => {
                        format!("↳ Running {} · thinking", stats)
                    }
                    _ => format!("↳ Running {}", stats),
                }
            }
        };
        Some(line)
    }

    pub fn thinking(content: impl Into<String>) -> Self {
        let content = content.into();
        let mut message = Self {
            id: next_message_id(),
            role: Role::Assistant,
            blocks: Vec::new(),
            raw: String::new(),
            kind: MessageKind::Thinking {
                content: content.clone(),
                duration_ms: None,
                expanded: false,
                user_pinned: false,
            },
            delivery: DeliveryStatus::default(),
            origin: UserMessageOrigin::Chat,
            provider: None,
            model: None,
            turn: None,
        };
        message.raw = content;
        message.blocks = parse_blocks(&message.raw);
        message
    }

    pub fn is_thinking(&self) -> bool {
        matches!(self.kind, MessageKind::Thinking { .. })
    }

    /// Whether this message is a harness notice (error / turn-pause / status).
    pub fn is_notice(&self) -> bool {
        matches!(self.kind, MessageKind::Notice { .. })
    }

    /// Construct a notice message. Replaces the ad-hoc
    /// `TranscriptMessage::new(Role::System, format!("Error: …"))` pattern with
    /// a typed severity so the renderer can pick color/icon from one place.
    pub fn notice(severity: NoticeSeverity, raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let blocks = parse_blocks(&raw);
        Self {
            id: next_message_id(),
            role: Role::System,
            blocks,
            raw,
            kind: MessageKind::Notice { severity },
            delivery: DeliveryStatus::default(),
            origin: UserMessageOrigin::Chat,
            provider: None,
            model: None,
            turn: None,
        }
    }

    /// A reasoning trace that has not yet been stamped with a duration — i.e.
    /// its stream is still open. The renderer treats this as the "spinner
    /// should keep breathing" state, and `finalize_streaming_reasoning` uses
    /// it to find orphaned traces to freeze after an interrupt.
    pub fn is_thinking_streaming(&self) -> bool {
        matches!(
            self.kind,
            MessageKind::Thinking {
                duration_ms: None,
                ..
            }
        )
    }

    pub fn thinking_expanded(&self) -> Option<bool> {
        match &self.kind {
            MessageKind::Thinking { expanded, .. } => Some(*expanded),
            _ => None,
        }
    }

    /// Auto/system disclosure setter — respects a user pin. See
    /// [`Self::set_tool_step_expanded`] for the rationale.
    pub fn set_thinking_expanded(&mut self, expanded: bool) {
        if let MessageKind::Thinking {
            expanded: current,
            user_pinned,
            ..
        } = &mut self.kind
        {
            if *user_pinned {
                return;
            }
            *current = expanded;
        }
    }

    /// User-driven disclosure change: force `expanded` and pin it.
    pub fn pin_thinking_expanded(&mut self, expanded: bool) {
        if let MessageKind::Thinking {
            expanded: current,
            user_pinned,
            ..
        } = &mut self.kind
        {
            *current = expanded;
            *user_pinned = true;
        }
    }

    pub fn set_thinking_duration(&mut self, duration_ms: u64) {
        if let MessageKind::Thinking { duration_ms: d, .. } = &mut self.kind {
            *d = Some(duration_ms);
        }
    }

    /// Human-readable summary for the reasoning trace (always one line).
    pub fn thinking_summary(&self) -> Option<String> {
        let MessageKind::Thinking {
            content,
            duration_ms,
            ..
        } = &self.kind
        else {
            return None;
        };
        let chars = content.chars().count();
        Some(match duration_ms {
            None => format!("Thinking · {} chars", chars),
            Some(_) => format!(
                "Thinking · {} chars · {}",
                chars,
                duration_text(*duration_ms)
            ),
        })
    }

    /// Human-readable header for the tool step (always one line).
    ///
    /// Shows only what the tool did and a duration suffix for finished
    /// states — the technical tool name lives inside the expanded body to
    /// reduce cognitive load.
    pub fn tool_step_summary(&self) -> Option<String> {
        let MessageKind::ToolStep {
            name,
            profile,
            arguments,
            status,
            duration_ms,
            ..
        } = &self.kind
        else {
            return None;
        };
        let summary = crate::render::tools::summary_for(name, arguments, profile.as_deref());
        Some(match status {
            ToolStepStatus::Running => summary,
            ToolStepStatus::Ok => format!("{} · {}", summary, duration_text(*duration_ms)),
            ToolStepStatus::Failed => {
                format!("{} · failed {}", summary, duration_text(*duration_ms))
            }
            ToolStepStatus::Denied => {
                format!("{} · denied {}", summary, duration_text(*duration_ms))
            }
            ToolStepStatus::Cancelled => {
                format!("{} · cancelled {}", summary, duration_text(*duration_ms))
            }
        })
    }

    fn refresh_tool_step(&mut self) {
        let MessageKind::ToolStep {
            id: _,
            name,
            profile,
            arguments,
            output,
            structured: _,
            status,
            expanded,
            user_pinned: _,
            duration_ms,
            started_at: _,
            children: _,
        } = &self.kind
        else {
            return;
        };
        if *expanded {
            // Expanded tool-step bodies are rendered directly from the
            // structured data (see draw_tool_step), not from parsed
            // markdown. We still populate `blocks` so semantic selection and
            // copy work: block 0 = display arguments, block 1 = output.
            let kv = parse_arguments_kv(arguments);
            let display_args: String = kv
                .iter()
                .map(|(k, v)| format!("{}: {}", k, v))
                .collect::<Vec<_>>()
                .join("\n");
            self.raw = display_args.clone();
            let mut blocks = vec![Block::Text {
                content: display_args,
                code_ranges: Vec::new(),
                bold_ranges: Vec::new(),
            }];
            if let Some(out) = output {
                self.raw.push_str("\n\n");
                self.raw.push_str(out);
                blocks.push(Block::Text {
                    content: out.clone(),
                    code_ranges: Vec::new(),
                    bold_ranges: Vec::new(),
                });
            }
            self.blocks = blocks;
        } else {
            let summary = crate::render::tools::summary_for(name, arguments, profile.as_deref());
            let suffix = match status {
                ToolStepStatus::Running => String::new(),
                ToolStepStatus::Ok => format!(" · {}", duration_text(*duration_ms)),
                ToolStepStatus::Failed => format!(" · failed {}", duration_text(*duration_ms)),
                ToolStepStatus::Denied => format!(" · denied {}", duration_text(*duration_ms)),
                ToolStepStatus::Cancelled => {
                    format!(" · cancelled {}", duration_text(*duration_ms))
                }
            };
            self.raw = format!("{}{}", summary, suffix);
            self.blocks = parse_blocks(&self.raw);
        }
    }

    /// Re-parse blocks from raw text (e.g. after streaming append).
    pub fn reparse(&mut self) {
        self.blocks = parse_blocks(&self.raw);
    }

    /// Append streaming text and re-parse.
    ///
    /// Parsing every accumulated chunk keeps the live layout structurally
    /// consistent with the final layout. The previous append-only Text block
    /// path delayed all Markdown structure until StreamEnd, causing the whole
    /// response to jump when headings, lists, and code fences were discovered.
    pub fn push_stream(&mut self, delta: &str) {
        self.raw.push_str(&sanitize_text(delta));
        self.reparse();
    }
}

/// Strip control characters (except \n, \t) to prevent Ratatui from rendering
/// them as block characters (█).
fn sanitize_text(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains(|c: char| c.is_control() && c != '\n' && c != '\t') {
        std::borrow::Cow::Owned(
            text.replace(|c: char| c.is_control() && c != '\n' && c != '\t', ""),
        )
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// Parse a JSON arguments string into ordered `(key, display_value)` pairs
/// suitable for compact rendering in the tool step body.
///
/// String values are shown unquoted; other JSON types keep their native
/// representation. Non-JSON input falls back to a single pair.
pub fn parse_arguments_kv(arguments: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return vec![("raw".to_string(), arguments.trim().to_string())];
    };
    let Some(object) = value.as_object() else {
        return vec![("value".to_string(), arguments.trim().to_string())];
    };
    object
        .iter()
        .map(|(key, val)| {
            let display = match val {
                serde_json::Value::String(s) => s.clone(),
                _ => val.to_string(),
            };
            (key.clone(), display)
        })
        .collect()
}

fn duration_text(duration_ms: Option<u64>) -> String {
    match duration_ms {
        None => "...".to_string(),
        Some(ms) if ms < 1000 => format!("{}ms", ms),
        Some(ms) if ms < 60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        Some(ms) => {
            let total_secs = ms / 1000;
            let h = total_secs / 3600;
            let m = (total_secs % 3600) / 60;
            let s = total_secs % 60;
            if h > 0 {
                format!("{}h {}m", h, m)
            } else {
                format!("{}m {}s", m, s)
            }
        }
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{}...", prefix)
    } else {
        prefix
    }
}

/// Parse raw markdown-like text into semantic blocks.
///
/// This is intentionally lightweight — it splits on major block boundaries
/// (code fences, headings, rules, blockquotes) while preserving the original
/// text so copying yields exact source.
pub fn parse_blocks(text: &str) -> Vec<Block> {
    parse_blocks_markdown(text)
}

/// Parse plain-text input (user messages) into blocks without any markdown
/// interpretation. The entire text becomes a single [`Block::Text`] so it
/// renders as one continuous verbatim panel; line breaks are preserved by the
/// renderer's wrapper rather than being collapsed by a markdown parser.
fn parse_blocks_plain(text: &str) -> Vec<Block> {
    if text.is_empty() {
        return Vec::new();
    }
    vec![Block::Text {
        content: text.to_string(),
        code_ranges: Vec::new(),
        bold_ranges: Vec::new(),
    }]
}

fn parse_blocks_markdown(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;

    // Accumulator for a paragraph: the prose lines (already stripped of their
    // block-prefix), joined with soft-break→space / hard-break→`\n` rules.
    // Once a paragraph is flushed we scan the resulting string for inline
    // `code` / `**bold**` runs and record their byte ranges.
    let mut para: Vec<String> = Vec::new();
    let mut para_hard: Vec<bool> = Vec::new(); // hard-break before this line?

    // (List items are pushed directly during the list run — adjacent items
    // share no Break thanks to push_block's ListItem-pair rule.)

    let flush_para =
        |para: &mut Vec<String>, para_hard: &mut Vec<bool>, blocks: &mut Vec<Block>| {
            if para.is_empty() {
                return;
            }
            // Join lines: a soft break inserts a space; a hard break (the *previous*
            // line ended with a two-space marker) inserts a literal "\n".
            let mut content = String::new();
            for (idx, line) in para.iter().enumerate() {
                if idx > 0 {
                    content.push(if para_hard[idx - 1] { '\n' } else { ' ' });
                }
                content.push_str(line);
            }
            let (code_ranges, bold_ranges) = scan_inline(&content);
            let trimmed_len = content.trim_end().len();
            let content = content[..trimmed_len].to_string();
            push_block(
                blocks,
                Block::Text {
                    content,
                    code_ranges: clamp_ranges(&code_ranges, trimmed_len),
                    bold_ranges: clamp_ranges(&bold_ranges, trimmed_len),
                },
            );
            para.clear();
            para_hard.clear();
        };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // --- Fenced code block ------------------------------------------------
        if let Some(rest) = trimmed.strip_prefix("```") {
            flush_para(&mut para, &mut para_hard, &mut blocks);
            let lang = rest.trim().to_string();
            let language = if lang.is_empty() { None } else { Some(lang) };
            let mut content = String::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(lines[i]);
                i += 1;
            }
            // skip closing fence (if present)
            if i < lines.len() {
                i += 1;
            }
            push_block(&mut blocks, Block::Code { language, content });
            continue;
        }

        // --- Horizontal rule --------------------------------------------------
        if is_rule(trimmed) {
            flush_para(&mut para, &mut para_hard, &mut blocks);
            push_block(&mut blocks, Block::Rule);
            i += 1;
            continue;
        }

        // --- Heading ----------------------------------------------------------
        if let Some((level, content_line)) = parse_heading(trimmed) {
            flush_para(&mut para, &mut para_hard, &mut blocks);
            let (code_ranges, bold_ranges) = scan_inline(content_line);
            let trimmed_len = content_line.trim_end().len();
            push_block(
                &mut blocks,
                Block::Heading {
                    level,
                    content: content_line[..trimmed_len].to_string(),
                    code_ranges: clamp_ranges(&code_ranges, trimmed_len),
                    bold_ranges: clamp_ranges(&bold_ranges, trimmed_len),
                },
            );
            i += 1;
            continue;
        }

        // --- Blockquote -------------------------------------------------------
        if let Some(content_line) = parse_quote(trimmed) {
            flush_para(&mut para, &mut para_hard, &mut blocks);
            // Collect consecutive quote lines.
            let mut q_lines: Vec<String> = Vec::new();
            let mut q_hard: Vec<bool> = Vec::new();
            q_lines.push(content_line.to_string());
            q_hard.push(false);
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim_start();
                if let Some(c) = parse_quote(t) {
                    let hard = line_ends_hard(q_lines.last().unwrap());
                    q_hard.push(hard);
                    q_lines.push(c.to_string());
                    i += 1;
                } else {
                    break;
                }
            }
            let mut content = String::new();
            for (idx, l) in q_lines.iter().enumerate() {
                if idx > 0 {
                    content.push(if q_hard[idx] { '\n' } else { ' ' });
                }
                content.push_str(l);
            }
            let (code_ranges, bold_ranges) = scan_inline(&content);
            let trimmed_len = content.trim_end().len();
            push_block(
                &mut blocks,
                Block::Quote {
                    content: content[..trimmed_len].to_string(),
                    code_ranges: clamp_ranges(&code_ranges, trimmed_len),
                    bold_ranges: clamp_ranges(&bold_ranges, trimmed_len),
                },
            );
            continue;
        }

        // --- List item --------------------------------------------------------
        if parse_list_item(trimmed).is_some() {
            flush_para(&mut para, &mut para_hard, &mut blocks);
            // Collect consecutive list items as a group; push_block's
            // ListItem↔ListItem rule keeps them tight (no Break between).
            while i < lines.len() {
                let t = lines[i].trim_start();
                if let Some((m, c, ch)) = parse_list_item(t) {
                    let (code_ranges, bold_ranges) = scan_inline(c);
                    let trimmed_len = c.trim_end().len();
                    push_block(
                        &mut blocks,
                        Block::ListItem {
                            content: c[..trimmed_len].to_string(),
                            code_ranges: clamp_ranges(&code_ranges, trimmed_len),
                            bold_ranges: clamp_ranges(&bold_ranges, trimmed_len),
                            ordered: m,
                            depth: 0,
                            checked: ch,
                        },
                    );
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }

        // --- Table (GFM: | ... | lines with a separator row) ------------------
        if trimmed.starts_with('|')
            && i + 1 < lines.len()
            && is_table_separator(lines[i + 1].trim())
        {
            flush_para(&mut para, &mut para_hard, &mut blocks);
            let mut table = TableAccumulator::default();
            // Header row
            let header_cells = split_table_row(trimmed);
            table.header = header_cells.clone();
            // Alignment from separator
            table.aligns = parse_table_aligns(lines[i + 1].trim());
            i += 2;
            // Body rows
            while i < lines.len() {
                let t = lines[i].trim();
                if t.starts_with('|') && !is_table_separator(t) {
                    let cells = split_table_row(t);
                    table.rows.push(cells);
                    i += 1;
                } else {
                    break;
                }
            }
            // GFM tables define the column count from the header: a body row
            // with fewer cells is padded with empty cells, and a row with more
            // is truncated. Normalizing here establishes the invariant that
            // every row in `Block::Table` has exactly `headers.len()` cells, so
            // every consumer (live renderer, selection copy, hit-testing) can
            // index a row by column without per-access bounds checks. Without
            // this, a ragged body row panicked the adaptive renderer (index out
            // of bounds in `build_table_render`).
            normalize_table_rows(&table.header, &mut table.rows);
            let rendered = table.render();
            if !rendered.is_empty() {
                push_block(
                    &mut blocks,
                    Block::Table {
                        headers: table.header,
                        rows: table.rows,
                        aligns: table.aligns,
                        rendered,
                    },
                );
            }
            continue;
        }

        // --- Blank line: paragraph break -------------------------------------
        if trimmed.is_empty() {
            flush_para(&mut para, &mut para_hard, &mut blocks);
            i += 1;
            continue;
        }

        // --- Ordinary prose line ---------------------------------------------
        // A trailing two-space (or tab) marker is a hard line break. Strip it
        // from the stored text; the `para_hard` flag records that this line
        // ends in a hard break so the join inserts a literal "\n" before the
        // *next* line.
        let hard = line_ends_hard(line);
        let stored = trimmed.trim_end_matches([' ', '\t']);
        para.push(stored.to_string());
        para_hard.push(hard);
        i += 1;
    }

    flush_para(&mut para, &mut para_hard, &mut blocks);

    // Strip trailing Breaks (a trailing blank line should not produce one).
    while matches!(blocks.last(), Some(Block::Break)) {
        blocks.pop();
    }
    blocks
}

/// Whether a line is a thematic break (`---`, `***`, `___` with ≥3 same chars).
fn is_rule(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 3 {
        return false;
    }
    let c = s.chars().next().unwrap();
    if c != '-' && c != '*' && c != '_' {
        return false;
    }
    s.chars().all(|ch| ch == c) && s.chars().count() >= 3
}

/// Parse a heading line `# title` … `###### title`. Returns `(level, content)`
/// where `content` still carries any inline formatting markers.
fn parse_heading(s: &str) -> Option<(u8, &str)> {
    let hashes = s.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &s[hashes..];
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    if rest.is_empty() && !s[..hashes].chars().all(|c| c == '#') {
        return None;
    }
    Some((hashes as u8, rest))
}

/// Parse a blockquote line `> text`. Supports `> text` and `>text`.
fn parse_quote(s: &str) -> Option<&str> {
    s.strip_prefix('>')
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
}

/// Parse a list-item line. Returns `(ordered_marker, content, checked)`.
/// `ordered_marker` is `Some(n)` for `N. `, `None` for bullet (`-`/`*`/`+ `).
/// `checked` is `Some(bool)` for task-list items `- [x]`/`- [ ]`.
fn parse_list_item(s: &str) -> Option<(Option<u64>, &str, Option<bool>)> {
    // Task list: - [x] / - [ ] / * [x] / + [ ]
    if let Some(after_bullet) = strip_bullet(s) {
        let after = after_bullet.trim_start_matches(' ');
        if let Some(rest) = after.strip_prefix("[") {
            let rest_first = rest.chars().next();
            let checked = match rest_first {
                Some('x') | Some('X') => Some(true),
                Some(' ') => Some(false),
                _ => None,
            };
            if checked.is_some()
                && let Some(content) = rest[1..].strip_prefix("]")
            {
                return Some((None, content.trim_start(), checked));
            }
        }
        return Some((None, after, None));
    }
    // Ordered list: 1. / 2. …
    if let Some((num, rest)) = parse_ordered(s) {
        let rest = rest.trim_start_matches(' ');
        // Ordered task list: 1. [x] (rare, but handle it)
        if let Some(r) = rest.strip_prefix("[") {
            let checked = match r.chars().next() {
                Some('x') | Some('X') => Some(true),
                Some(' ') => Some(false),
                _ => None,
            };
            if checked.is_some()
                && let Some(content) = r[1..].strip_prefix("]")
            {
                return Some((Some(num), content.trim_start(), checked));
            }
        }
        return Some((Some(num), rest, None));
    }
    None
}

/// Strip a bullet prefix (`-`/`*`/`+`), returning the remainder.
fn strip_bullet(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix("- ") {
        Some(rest)
    } else if let Some(rest) = s.strip_prefix("* ") {
        Some(rest)
    } else if let Some(rest) = s.strip_prefix("+ ") {
        Some(rest)
    } else if let Some(rest) = s.strip_prefix("-\t") {
        Some(rest)
    } else {
        None
    }
}

/// Parse an ordered-list marker `N. ` or `N) `, returning `(N, remainder)`.
fn parse_ordered(s: &str) -> Option<(u64, &str)> {
    let digits_end = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits_end == 0 {
        return None;
    }
    let rest = &s[digits_end..];
    if let Some(after) = rest.strip_prefix(". ") {
        let n: u64 = s[..digits_end].parse().ok()?;
        return Some((n, after));
    }
    if let Some(after) = rest.strip_prefix(") ") {
        let n: u64 = s[..digits_end].parse().ok()?;
        return Some((n, after));
    }
    None
}

/// Whether a line ends with a hard break (≥2 trailing spaces). The two-space
/// marker is stripped from the content before this is called on the stored
/// string, so we check the *original* line; callers pass the raw line.
fn line_ends_hard(line: &str) -> bool {
    line.ends_with("  ") || line.ends_with("\t")
}

/// Is this line a GFM table separator (`| --- | :--: | ---: |`)?
fn is_table_separator(s: &str) -> bool {
    if !s.contains('-') {
        return false;
    }
    let stripped = s.trim_matches('|').trim();
    if stripped.is_empty() {
        return false;
    }
    // Each cell must contain at least one `-`, only `-`,`:`,and spaces.
    stripped.split('|').all(|cell| {
        let c = cell.trim();
        !c.is_empty() && c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')
    })
}

/// Parse alignment markers from a separator row into `TableAlignment`s.
fn parse_table_aligns(sep: &str) -> Vec<TableAlignment> {
    sep.trim_matches('|')
        .split('|')
        .map(|cell| {
            let c = cell.trim();
            let left = c.starts_with(':');
            let right = c.ends_with(':');
            match (left, right) {
                (true, true) => TableAlignment::Center,
                (true, false) => TableAlignment::Left,
                (false, true) => TableAlignment::Right,
                (false, false) => TableAlignment::None,
            }
        })
        .collect()
}

/// Split a `| a | b | c |` row into trimmed cell strings.
fn split_table_row(line: &str) -> Vec<String> {
    let line = line.trim();
    // Strip leading/trailing `|`.
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    line.split('|').map(|c| c.trim().to_string()).collect()
}

/// Scan a prose string for inline `` `code` `` and `**bold**` runs, returning
/// `(code_ranges, bold_ranges)` as byte offsets `[start, end)` into `content`.
/// Delimiters are *kept* in `content` (the caller owns the string), so the
/// ranges cover the full marker-inclusive span — matching the contract that
/// copy/selection see plain text and rendering paints over the quoted region.
pub fn scan_inline(content: &str) -> (Vec<CodeRange>, Vec<CodeRange>) {
    let bytes = content.as_bytes();
    let mut code_ranges = Vec::new();
    let mut bold_ranges = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        // Inline code: a run of backticks, closed by the same number.
        if bytes[i] == b'`' {
            let tick_count = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let close_start = i + tick_count;
            // Find a matching-length closing fence.
            if let Some(rel) = find_backtick_run(&content[close_start..], tick_count) {
                let end = close_start + rel + tick_count;
                code_ranges.push((i, end));
                i = end;
                continue;
            }
        }
        // Bold: `**…**`.
        if bytes[i] == b'*'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'*'
            && let Some(rel) = content[i + 2..].find("**")
        {
            let end = i + 2 + rel + 2;
            bold_ranges.push((i, end));
            i = end;
            continue;
        }
        i += 1;
    }
    (code_ranges, bold_ranges)
}

/// Find the byte offset of a run of exactly `n` backticks within `s`.
fn find_backtick_run(s: &str, n: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + n <= bytes.len() {
        if bytes[i..i + n].iter().all(|&b| b == b'`') {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Enforce the GFM table column-count invariant: the number of columns is
/// fixed by the header row, so every body row is normalized to exactly that
/// width — short rows are padded with empty cells, over-wide rows truncated.
/// Establishing this once at parse time lets every consumer index rows by
/// column without per-access bounds checks.
fn normalize_table_rows(header: &[String], rows: &mut [Vec<String>]) {
    let ncols = header.len();
    if ncols == 0 {
        // Degenerate: no columns to normalize against. Such a table yields an
        // empty render and is dropped by the caller, so the rows are unused.
        return;
    }
    for row in rows {
        if row.len() > ncols {
            row.truncate(ncols);
        } else if row.len() < ncols {
            row.resize(ncols, String::new());
        }
    }
}

#[derive(Default)]
struct TableAccumulator {
    aligns: Vec<TableAlignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl TableAccumulator {
    /// Render the table as a GFM-style aligned grid using box-drawing borders.
    ///
    /// Columns are sized to their widest cell (intrinsic width) so vertical
    /// separators line up across all rows. The header is followed by a
    /// separator rule. Wide tables that exceed the viewport are handed to the
    /// renderer's normal line wrapping rather than being truncated.
    fn render(&self) -> String {
        if self.header.is_empty() {
            return String::new();
        }
        let ncols = self.header.len();
        let width = |cell: &str| display_width(cell);

        // Per-column intrinsic width: max of header and every body cell.
        // Rows are pre-normalized to `ncols` cells by `normalize_table_rows`,
        // so iterating in full here touches exactly one cell per column.
        let mut widths = vec![0usize; ncols];
        for (i, h) in self.header.iter().enumerate() {
            widths[i] = widths[i].max(width(h));
        }
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(width(cell));
            }
        }

        let join_borders = |sep: &str| -> String {
            widths
                .iter()
                .map(|w| "─".repeat(w + 2))
                .collect::<Vec<_>>()
                .join(sep)
        };

        let mut out = String::new();
        out.push_str(&format!("┌{}┐\n", join_borders("┬")));
        out.push_str(&format_row(&self.header, &widths, &self.aligns));
        out.push('\n');
        out.push_str(&format!("├{}┤\n", join_borders("┼")));
        for row in &self.rows {
            out.push_str(&format_row(row, &widths, &self.aligns));
            out.push('\n');
        }
        out.push_str(&format!("└{}┘", join_borders("┴")));
        out
    }
}

/// Format one table row as `│ cell │ cell │`, honoring per-column alignment.
fn format_row(cells: &[String], widths: &[usize], aligns: &[TableAlignment]) -> String {
    let ncols = widths.len();
    let parts: Vec<String> = (0..ncols)
        .map(|i| {
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            let align = aligns.get(i).copied().unwrap_or(TableAlignment::None);
            pad_cell(cell, widths[i], align)
        })
        .collect();
    format!("│ {} │", parts.join(" │ "))
}

fn pad_cell(cell: &str, width: usize, align: TableAlignment) -> String {
    let cell_w = display_width(cell);
    let pad = width.saturating_sub(cell_w);
    match align {
        TableAlignment::Right => format!("{}{}", " ".repeat(pad), cell),
        TableAlignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), cell, " ".repeat(right))
        }
        TableAlignment::None | TableAlignment::Left => format!("{}{}", cell, " ".repeat(pad)),
    }
}

fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Drop ranges that fall entirely past `len` and clamp the end of any range
/// that straddles it (trim_end can only shrink trailing whitespace, so in
/// practice this is a no-op for interior code runs, but it keeps the invariant
/// `end <= content.len()` airtight).
fn clamp_ranges(ranges: &[CodeRange], len: usize) -> Vec<CodeRange> {
    ranges
        .iter()
        .map(|&(s, e)| (s.min(len), e.min(len)))
        .filter(|&(s, e)| s < e)
        .collect()
}

fn push_block(blocks: &mut Vec<Block>, block: Block) {
    if block.is_empty() && !matches!(block, Block::Rule | Block::Break) {
        return;
    }
    let needs_gap = blocks.last().is_some_and(|previous| {
        !matches!(
            (previous, &block),
            (Block::Break, _) | (Block::ListItem { .. }, Block::ListItem { .. })
        )
    });
    if needs_gap {
        blocks.push(Block::Break);
    }
    blocks.push(block);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_text() {
        let blocks = parse_blocks("Hello world");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], Block::Text { content, .. } if content == "Hello world"));
    }

    #[test]
    fn test_parse_code_block() {
        let text = "Some text\n\n```rust\nfn main() {}\n```\n\nMore text";
        let blocks = parse_blocks(text);
        assert_eq!(blocks.len(), 5);
        assert!(matches!(&blocks[0], Block::Text { content, .. } if content == "Some text"));
        assert!(
            matches!(&blocks[2], Block::Code { language, content } if language.as_deref() == Some("rust") && content == "fn main() {}")
        );
        assert!(matches!(&blocks[4], Block::Text { content, .. } if content == "More text"));
    }

    #[test]
    fn inline_code_keeps_its_backtick_quotes_in_prose() {
        // Inline code keeps its backtick delimiters in the flattened content
        // so the rendered/copied paragraph still shows the quotes, and the
        // renderer can paint the span on the code surface. This holds across
        // paragraph / heading / list item / quote contexts.
        let blocks = parse_blocks("Call the `read_text` tool.");
        assert!(matches!(
            &blocks[0],
            Block::Text { content, .. } if content == "Call the `read_text` tool."
        ));

        // Heading.
        let blocks = parse_blocks("# Use `list_dir` for directories");
        assert!(matches!(
            &blocks[0],
            Block::Heading { content, level: 1, .. } if content == "Use `list_dir` for directories"
        ));

        // List item.
        let blocks = parse_blocks("- item with `code` inside");
        assert!(matches!(
            &blocks[0],
            Block::ListItem { content, .. } if content == "item with `code` inside"
        ));

        // Blockquote.
        let blocks = parse_blocks("> quoted `code` span");
        assert!(matches!(
            &blocks[0],
            Block::Quote { content, .. } if content == "quoted `code` span"
        ));

        // Multiple inline spans in one paragraph, mixed with emphasis.
        let blocks = parse_blocks("Mix `a` and `b` and plain.");
        assert!(matches!(
            &blocks[0],
            Block::Text { content, .. } if content == "Mix `a` and `b` and plain."
        ));
    }

    /// Helper: find the byte range of the first `` `…` `` run in `s`, matching
    /// what the parser records, so the `code_ranges` assertions below can be
    /// written against the literal content rather than hand-counted offsets.
    fn code_ranges_of(s: &str) -> Vec<CodeRange> {
        let mut ranges = Vec::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'`' {
                // find the closing backtick
                if let Some(rel) = s[i + 1..].find('`') {
                    ranges.push((i, i + 1 + rel + 1));
                    i = i + 1 + rel + 1;
                    continue;
                }
            }
            i += 1;
        }
        ranges
    }

    #[test]
    fn inline_code_records_byte_ranges_for_every_prose_context() {
        // Paragraph: the run is `read_text` including both backticks.
        let text = "Call the `read_text` tool.";
        let expected = code_ranges_of(text);
        let blocks = parse_blocks(text);
        let Block::Text {
            content,
            code_ranges,
            ..
        } = &blocks[0]
        else {
            panic!("expected Text block, got {:?}", blocks[0]);
        };
        assert_eq!(content, text);
        assert_eq!(code_ranges, &expected);

        // Heading.
        let text = "Use `list_dir` for directories";
        let expected = code_ranges_of(text);
        let blocks = parse_blocks(&format!("# {text}"));
        let Block::Heading {
            content,
            code_ranges,
            ..
        } = &blocks[0]
        else {
            panic!("expected Heading block, got {:?}", blocks[0]);
        };
        assert_eq!(content, text);
        assert_eq!(code_ranges, &expected);

        // List item.
        let text = "item with `code` inside";
        let expected = code_ranges_of(text);
        let blocks = parse_blocks(&format!("- {text}"));
        let Block::ListItem {
            content,
            code_ranges,
            ..
        } = &blocks[0]
        else {
            panic!("expected ListItem block, got {:?}", blocks[0]);
        };
        assert_eq!(content, text);
        assert_eq!(code_ranges, &expected);

        // Blockquote.
        let text = "quoted `code` span";
        let expected = code_ranges_of(text);
        let blocks = parse_blocks(&format!("> {text}"));
        let Block::Quote {
            content,
            code_ranges,
            ..
        } = &blocks[0]
        else {
            panic!("expected Quote block, got {:?}", blocks[0]);
        };
        assert_eq!(content, text);
        assert_eq!(code_ranges, &expected);

        // Multiple spans → multiple, non-overlapping, ordered ranges.
        let text = "Mix `a` and `b` and plain.";
        let expected = code_ranges_of(text);
        let blocks = parse_blocks(text);
        let Block::Text { code_ranges, .. } = &blocks[0] else {
            panic!("expected Text block");
        };
        assert_eq!(code_ranges, &expected);
    }

    #[test]
    fn test_push_stream() {
        let mut streamed = TranscriptMessage::new(Role::Assistant, "");
        for chunk in [
            "# Result\n\n",
            "First paragraph.\n\n",
            "- one\n",
            "- two\n\n",
            "```rust\nfn main() {}\n```",
        ] {
            streamed.push_stream(chunk);
        }

        let completed = TranscriptMessage::new(Role::Assistant, streamed.raw.clone());
        assert_eq!(streamed.blocks, completed.blocks);
    }

    #[test]
    fn parses_block_boundaries_without_collapsing_the_document() {
        let blocks = parse_blocks(
            "# Result\n\nFirst paragraph.\n\nSecond paragraph.\n\n1. one\n2. two\n\n> quoted",
        );

        assert!(matches!(
            &blocks[0],
            Block::Heading { level: 1, content, .. } if content == "Result"
        ));
        assert!(blocks.iter().any(|block| matches!(block, Block::Break)));
        assert!(blocks.iter().any(
            |block| matches!(block, Block::Text { content, .. } if content == "First paragraph.")
        ));
        assert!(blocks.iter().any(
            |block| matches!(block, Block::Text { content, .. } if content == "Second paragraph.")
        ));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::ListItem {
                content,
                ordered: Some(1),
                ..
            } if content == "one"
        )));
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::Quote { content, .. } if content == "quoted"))
        );
    }

    #[test]
    fn headings_are_visually_separated_from_following_body_text() {
        let blocks = parse_blocks("# Result\nFirst paragraph.");

        assert!(matches!(&blocks[0], Block::Heading { content, .. } if content == "Result"));
        assert!(
            matches!(&blocks[1], Block::Break),
            "heading-to-text boundaries should render with a blank row"
        );
        assert!(matches!(&blocks[2], Block::Text { content, .. } if content == "First paragraph."));
    }

    #[test]
    fn markdown_soft_breaks_flow_but_hard_breaks_are_preserved() {
        let soft = parse_blocks("alpha bravo\ncharlie delta");
        assert!(matches!(
            &soft[0],
            Block::Text { content, .. } if content == "alpha bravo charlie delta"
        ));

        let hard = parse_blocks("alpha bravo  \ncharlie delta");
        assert!(matches!(
            &hard[0],
            Block::Text { content, .. } if content == "alpha bravo\ncharlie delta"
        ));
    }

    #[test]
    fn parses_task_lists_and_tables() {
        let blocks = parse_blocks(
            "- [x] done\n- [ ] next\n\n| Name | State |\n| --- | --- |\n| neenee | ready |",
        );

        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::ListItem {
                checked: Some(true),
                content,
                ..
            } if content == "done"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::ListItem {
                checked: Some(false),
                content,
                ..
            } if content == "next"
        )));
        let table = blocks.iter().find_map(|block| match block {
            Block::Table { headers, rows, .. } => Some((headers, rows)),
            _ => None,
        });
        let (headers, rows) = table.expect("table block present");
        assert_eq!(headers, &["Name".to_string(), "State".to_string()]);
        assert_eq!(rows, &[vec!["neenee".to_string(), "ready".to_string()]]);

        // The rendered grid must align columns and separate the header from
        // the body, the regression that motivated reintroducing Block::Table.
        let rendered = blocks
            .iter()
            .find_map(|block| match block {
                Block::Table { rendered, .. } => Some(rendered.as_str()),
                _ => None,
            })
            .expect("rendered table text");
        assert!(rendered.contains("┌"), "missing top border: {rendered}");
        assert!(
            rendered.contains("├"),
            "missing header/body separator: {rendered}"
        );
        // Pipes must line up: the header and data rows share the same `│`
        // positions, so splitting on `│` yields the same number of pieces.
        let pipes = |line: &str| line.matches('│').count();
        let header_line = rendered.lines().nth(1).unwrap();
        let data_line = rendered.lines().nth(3).unwrap();
        assert_eq!(
            pipes(header_line),
            pipes(data_line),
            "header and body rows must align: {rendered}"
        );
    }

    #[test]
    fn table_alignment_and_uneven_cells_line_up() {
        let blocks =
            parse_blocks("| Tool | Count |\n| :--- | ---: |\n| read | 1 |\n| webfetch | 250 |");
        let rendered = blocks
            .iter()
            .find_map(|block| match block {
                Block::Table {
                    rendered, aligns, ..
                } => Some((rendered.as_str(), aligns.clone())),
                _ => None,
            })
            .expect("table block");
        let (rendered, aligns) = rendered;
        assert_eq!(
            aligns,
            vec![TableAlignment::Left, TableAlignment::Right],
            "alignment must be captured: {rendered}"
        );
        // Right-aligned numeric column: digits hug the right border, so the
        // single-digit "1" gets more left padding than "250" does.
        let data_lines: Vec<&str> = rendered.lines().skip(3).take(2).collect();
        assert!(
            data_lines[0].ends_with("│     1 │"),
            "got: {}",
            data_lines[0]
        );
        assert!(
            data_lines[1].ends_with("│   250 │"),
            "got: {}",
            data_lines[1]
        );
    }

    /// GFM fixes the table column count from the header, so every body row in
    /// a `Block::Table` must be normalized to exactly `headers.len()` cells:
    /// short rows padded with empty strings, over-wide rows truncated. This is
    /// the invariant the live renderer indexes against; a ragged row used to
    /// panic `build_table_render` with an out-of-bounds index.
    #[test]
    fn table_normalizes_ragged_body_rows_to_header_width() {
        // 2-column header; body rows have 2, 1, and 3 cells respectively.
        let blocks = parse_blocks("| A | B |\n|---|---|\n| 1 | 2 |\n| 3 |\n| 4 | 5 | 6 |");
        let (headers, rows) = blocks
            .iter()
            .find_map(|block| match block {
                Block::Table { headers, rows, .. } => Some((headers.clone(), rows.clone())),
                _ => None,
            })
            .expect("table block present");
        let ncols = headers.len();
        assert_eq!(ncols, 2, "header defines 2 columns");
        assert!(
            rows.iter().all(|row| row.len() == ncols),
            "every body row must be normalized to {ncols} cells, got {rows:?}"
        );
        // Short rows are padded with empty cells, the over-wide row truncated.
        assert_eq!(rows[0], vec!["1".to_string(), "2".to_string()]);
        assert_eq!(rows[1], vec!["3".to_string(), String::new()]);
        assert_eq!(rows[2], vec!["4".to_string(), "5".to_string()]);
    }

    #[test]
    fn tool_step_collapses_and_restores_full_semantic_detail() {
        let mut message =
            TranscriptMessage::tool_step("call_1", "read_text", r#"{"path":"README.md"}"#);
        // Collapsed running: human-readable summary only — no tool name.
        assert!(message.raw.contains("Read README.md"));
        assert!(!message.raw.contains("read_text"));

        assert!(message.finish_tool_step(
            "call_1",
            "contents",
            neenee_core::ToolOutput::text("contents"),
            1234
        ));
        // Collapsed completed: summary + duration suffix.
        assert!(message.raw.contains("Read README.md"));
        assert!(message.raw.contains("1.2s"));
        message.set_tool_step_expanded(true);

        // Expanded: arguments as compact key-value text + output verbatim.
        assert!(message.raw.contains("path: README.md"));
        assert!(message.raw.contains("contents"));
    }

    #[test]
    fn envoy_task_is_detected_and_addressable() {
        let task = TranscriptMessage::tool_step(
            "call_42",
            "envoy",
            r#"{"description":"explore src","prompt":"..."}"#,
        );
        assert!(task.is_envoy_task());
        assert_eq!(task.tool_step_call_id(), Some("call_42"));
        assert_eq!(task.envoy_children().map(|c| c.len()), Some(0));
        assert_eq!(task.envoy_label(), "explore src");

        // A regular tool step is not an envoy task.
        let read = TranscriptMessage::tool_step("call_1", "read_text", r#"{"path":"a"}"#);
        assert!(!read.is_envoy_task());
        assert!(read.envoy_status_line().is_none());
    }

    #[test]
    fn envoy_started_event_labels_step_by_role() {
        // A `Started` event stamps the bound profile name on the step so the
        // nav bar / collapsed summary read by role (`plan · …`) instead of a
        // generic "Envoy".
        let mut task = TranscriptMessage::tool_step(
            "call_7",
            "envoy",
            r#"{"description":"write the plan","prompt":"..."}"#,
        );
        assert_eq!(task.envoy_label(), "write the plan");
        assert!(task.push_envoy_event(&neenee_core::EnvoyEvent::Started {
            profile: "explore".to_string()
        }));
        assert_eq!(task.envoy_label(), "explore · write the plan");
        // The collapsed header picks the role up via `tool_step_summary` too.
        let header = task.tool_step_summary().expect("summary");
        assert!(
            header.starts_with("explore:"),
            "collapsed summary should lead with the role; got: {header}"
        );
    }

    #[test]
    fn envoy_status_reflects_children_and_completion() {
        let mut task =
            TranscriptMessage::tool_step("call_9", "envoy", r#"{"description":"d","prompt":"p"}"#);

        // No children yet, still running.
        let running = task.envoy_status_line().expect("running status");
        assert!(running.starts_with("↳ Running"), "got: {running}");

        // Streaming assistant text => a "thinking" suffix.
        task.push_envoy_event(&EnvoyEvent::StreamStart);
        task.push_envoy_event(&EnvoyEvent::StreamDelta("partial".into()));
        let thinking = task.envoy_status_line().expect("thinking status");
        assert!(thinking.starts_with("↳ Running"), "got: {thinking}");
        assert!(thinking.ends_with("thinking"), "got: {thinking}");

        // An in-flight child tool call surfaces the tool's header.
        task.push_envoy_event(&EnvoyEvent::ToolCall {
            id: "inner".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"foo"}"#.into(),
        });
        let running = task.envoy_status_line().expect("running status");
        assert!(running.starts_with("↳ Running"), "got: {running}");
        assert!(running.contains("Grep"), "got: {running}");

        // Completing the parent summarizes tool-call count + duration.
        assert!(task.finish_tool_step(
            "call_9",
            "final answer",
            neenee_core::ToolOutput::text("final answer"),
            1500
        ));
        let done = task.envoy_status_line().expect("done status");
        assert!(done.starts_with("↳ Completed"), "got: {done}");
        assert!(done.contains("1 tool calls"), "got: {done}");
        assert!(done.contains("1.5s"), "got: {done}");

        // Children are accessible for the dedicated envoy view.
        assert_eq!(task.envoy_children().map(|c| c.len()), Some(2));
    }

    #[test]
    fn envoy_failed_status_reports_failure() {
        let mut task =
            TranscriptMessage::tool_step("c", "envoy", r#"{"description":"d","prompt":"p"}"#);
        task.push_envoy_event(&EnvoyEvent::ToolCall {
            id: "i".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        // The envoy failure is now signalled by the structured `failed`
        // flag on `ToolOutput::Envoy`, not by an "Error:" text prefix.
        let structured = neenee_core::ToolOutput::Envoy {
            summary: "Error: boom".into(),
            messages: Vec::new(),
            usage: neenee_core::TokenUsage::default(),
            failed: true,
        };
        assert!(task.finish_tool_step("c", structured.to_text(), structured, 100));
        let status = task.envoy_status_line().unwrap();
        assert!(status.starts_with("↳ Failed"), "got: {status}");
    }

    #[test]
    fn bash_failure_is_classified_failed_from_structured_exit_code() {
        // Regression: a bash failure emits `Exit N …` which does NOT start with
        // "Error", so the legacy text sniff misclassified it as `Ok`. With
        // structured `ToolOutput::Shell { exit: Some(1) }`, `is_error()` now
        // drives the classification and the step correctly reads `Failed`.
        let mut step = TranscriptMessage::tool_step("c", "bash", r#"{"command":"false"}"#);
        let structured = neenee_core::ToolOutput::Shell {
            command: "false".into(),
            stdout: String::new(),
            stderr: "boom".into(),
            lines: Vec::new(),
            exit: Some(1),
            truncated: false,
            termination: neenee_core::tool_output::ShellTermination::Exited,
        };
        let text = structured.to_text();
        assert!(
            !text.starts_with("Error"),
            "precondition: text is not Error-prefixed"
        );
        assert!(step.finish_tool_step("c", text, structured, 50));
        assert_eq!(step.tool_step_status(), Some(ToolStepStatus::Failed));
    }

    #[test]
    fn bash_success_is_classified_ok() {
        let mut step = TranscriptMessage::tool_step("c", "bash", r#"{"command":"true"}"#);
        let structured = neenee_core::ToolOutput::Shell {
            command: "true".into(),
            stdout: "ok\n".into(),
            stderr: String::new(),
            lines: Vec::new(),
            exit: Some(0),
            truncated: false,
            termination: neenee_core::tool_output::ShellTermination::Exited,
        };
        let text = structured.to_text();
        assert!(step.finish_tool_step("c", text, structured, 5));
        assert_eq!(step.tool_step_status(), Some(ToolStepStatus::Ok));
    }

    #[test]
    fn push_tool_stream_builds_interleaved_lines_for_live_view() {
        // L5: the streaming seed must populate `lines` (with the right stream
        // tag each) so the live view renders arrival-ordered, stderr-tinted,
        // interleaved output — not the all-stdout-then-all-stderr degraded
        // band the empty-`lines` fallback forced.
        use neenee_core::{ToolStream, tool_output::ShellStream};
        let mut step = TranscriptMessage::tool_step("c", "bash", r#"{"command":"x"}"#);
        assert!(step.push_tool_stream("c", &ToolStream::Stdout("Compiling a\n".into())));
        assert!(step.push_tool_stream("c", &ToolStream::Stderr("warning: b\n".into())));
        assert!(step.push_tool_stream("c", &ToolStream::Stdout("Compiling c\n".into())));

        let lines = match &step.kind {
            MessageKind::ToolStep {
                structured: Some(b),
                ..
            } => match b.as_ref() {
                neenee_core::ToolOutput::Shell { lines, .. } => lines,
                _ => panic!("expected Shell"),
            },
            _ => panic!("expected ToolStep"),
        };
        assert_eq!(
            lines
                .iter()
                .map(|l| (l.stream, l.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (ShellStream::Out, "Compiling a"),
                (ShellStream::Err, "warning: b"),
                (ShellStream::Out, "Compiling c"),
            ],
            "streaming seed must preserve arrival order + stream tags"
        );
        // The flat strings stay populated too (model-facing path).
        match step.kind {
            MessageKind::ToolStep {
                structured: Some(b),
                ..
            } => match b.as_ref() {
                neenee_core::ToolOutput::Shell { stdout, stderr, .. } => {
                    assert!(stdout.contains("Compiling a"));
                    assert!(stdout.contains("Compiling c"));
                    assert!(stderr.contains("warning: b"));
                }
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn cancel_tool_step_transitions_to_a_terminal_state() {
        let mut step = TranscriptMessage::tool_step("call_1", "websearch", r#"{"query":"rust"}"#);
        // Running -> Cancelled is a real terminal transition.
        assert_eq!(step.tool_step_status(), Some(ToolStepStatus::Running));
        assert!(step.cancel_tool_step("call_1"));
        assert_eq!(step.tool_step_status(), Some(ToolStepStatus::Cancelled));

        // The summary advertises the cancelled state instead of staying blank.
        let summary = step.tool_step_summary().expect("summary");
        assert!(summary.contains("cancelled"), "got: {summary}");
        // The raw (collapsed) transcript line mirrors the summary.
        assert!(step.raw.contains("cancelled"), "got: {}", step.raw);

        // Cancelled is terminal: a late result or another cancel is ignored.
        assert!(!step.finish_tool_step(
            "call_1",
            "late result",
            neenee_core::ToolOutput::text("late result"),
            10
        ));
        assert!(!step.cancel_tool_step("call_1"));
        assert_eq!(step.tool_step_status(), Some(ToolStepStatus::Cancelled));
    }

    #[test]
    fn cancel_only_acts_on_the_matching_call_id() {
        let mut step = TranscriptMessage::tool_step("call_1", "websearch", "{}");
        // A different id does nothing and leaves the step running.
        assert!(!step.cancel_tool_step("call_9"));
        assert_eq!(step.tool_step_status(), Some(ToolStepStatus::Running));
    }

    #[test]
    fn cancelling_a_envoy_also_cancels_its_running_children() {
        let mut task =
            TranscriptMessage::tool_step("task_1", "envoy", r#"{"description":"d","prompt":"p"}"#);
        // A nested tool call still in flight.
        task.push_envoy_event(&EnvoyEvent::ToolCall {
            id: "inner".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"foo"}"#.into(),
        });
        let children = task.envoy_children().expect("has children");
        assert_eq!(
            children[0].tool_step_status(),
            Some(ToolStepStatus::Running)
        );

        // Interrupting the parent task cancels it AND the nested running child,
        // so the envoy view never shows a stuck "running" step.
        assert!(task.cancel_tool_step("task_1"));
        assert_eq!(task.tool_step_status(), Some(ToolStepStatus::Cancelled));
        let children = task.envoy_children().expect("has children");
        assert_eq!(
            children[0].tool_step_status(),
            Some(ToolStepStatus::Cancelled),
            "nested child must converge with the parent"
        );

        let status = task.envoy_status_line().expect("status line");
        assert!(status.starts_with("↳ Cancelled"), "got: {status}");
    }

    #[test]
    fn cancel_all_running_is_a_defensive_sweep_that_skips_terminal_steps() {
        let mut a = TranscriptMessage::tool_step("a", "read_text", "{}");
        let mut b = TranscriptMessage::tool_step("b", "read_text", "{}");
        // `b` already finished successfully; the sweep must not clobber it.
        assert!(b.finish_tool_step(
            "b",
            "contents",
            neenee_core::ToolOutput::text("contents"),
            5
        ));
        assert_eq!(b.tool_step_status(), Some(ToolStepStatus::Ok));

        // The sweep cancels a running step and is then a no-op on it.
        assert!(a.cancel_all_running());
        assert!(!a.cancel_all_running());
        assert_eq!(a.tool_step_status(), Some(ToolStepStatus::Cancelled));
        // A finished step is untouched by the sweep.
        assert!(!b.cancel_all_running());
        assert_eq!(b.tool_step_status(), Some(ToolStepStatus::Ok));
    }

    #[test]
    fn notice_carries_severity_and_is_classified_as_notice() {
        let n = TranscriptMessage::notice(NoticeSeverity::Error, "boom");
        assert!(n.is_notice());
        assert!(matches!(
            n.kind,
            MessageKind::Notice {
                severity: NoticeSeverity::Error
            }
        ));
        // The raw text is preserved verbatim for the renderer (no "Error: "
        // prefix injection — the glyph is the renderer's job).
        assert_eq!(n.raw, "boom");
        // A text message is not a notice.
        let plain = TranscriptMessage::new(Role::Assistant, "hi");
        assert!(!plain.is_notice());
    }

    #[test]
    fn user_message_origin_defaults_to_chat_and_can_be_overridden() {
        // A plain user message is a genuine chat prompt by default.
        let chat = TranscriptMessage::new(Role::User, "fix the bug");
        assert_eq!(chat.origin, UserMessageOrigin::Chat);

        // Slash commands and shell passthroughs tag themselves so the
        // Activity modal does not mistake them for the driving prompt.
        let slash = TranscriptMessage::new(Role::User, "/review working-tree")
            .with_origin(UserMessageOrigin::Slash);
        assert_eq!(slash.origin, UserMessageOrigin::Slash);

        let shell =
            TranscriptMessage::new(Role::User, "!ls -la").with_origin(UserMessageOrigin::Shell);
        assert_eq!(shell.origin, UserMessageOrigin::Shell);

        // with_origin is idempotent and does not depend on the text: a
        // genuine chat prompt that happens to start with '/' stays Slash only
        // when explicitly tagged, never inferred from text here.
        let explicit_chat = TranscriptMessage::new(Role::User, "/etc is a path")
            .with_origin(UserMessageOrigin::Chat);
        assert_eq!(explicit_chat.origin, UserMessageOrigin::Chat);
    }
}
