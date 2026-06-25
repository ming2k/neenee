//! Semantic document model for the TUI.
//!
//! Unlike storing raw strings, this model preserves the structure of messages
//! so that selection and copy operate on semantic units (blocks) rather than
//! terminal grid characters.

use neenee_core::{Role, SubagentEvent};

/// Lifecycle of a tool step, stored explicitly (not inferred from `output`)
/// so an aborted call has its own terminal state instead of being stuck in
/// "no output yet". This is the single source of truth for tool-step state —
/// the renderer classifies it into a [`crate::tui::render::tools::ToolStatus`].
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
        /// The bound subagent profile name (`explore` / `plan` / `verify` / …)
        /// for a subagent-spawning tool step, populated from the first
        /// `SubagentEvent::Started` and used to label the step by its role.
        /// `None` for non-subagent steps, or until the `Started` event lands.
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
        /// its `Subagent`/`Patch` variants) is large enough that an unboxed
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
        /// elapsed time while the call (or subagent) is still running.
        /// `Instant` is cheap to capture at construction time and is not
        /// serialized — session restore reconstructs finished steps without it.
        started_at: Option<std::time::Instant>,
        /// Child events emitted by a subagent spawned from this tool step.
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
    /// A terminal failure surfaced from the harness or a tool.
    Error,
}

/// Table column text alignment, mirrored from pulldown-cmark so the `Block`
/// type does not leak the parser dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlignment {
    None,
    Left,
    Center,
    Right,
}

/// A single semantic block within a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Plain text paragraph.
    Text { content: String },
    /// Inline or fenced code.
    Code {
        language: Option<String>,
        content: String,
    },
    /// A heading.
    Heading { level: u8, content: String },
    /// A list item, preserving its marker and nesting level.
    ListItem {
        content: String,
        ordered: Option<u64>,
        depth: usize,
        checked: Option<bool>,
    },
    /// A blockquote.
    Quote { content: String },
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
            Block::Text { content } => content,
            Block::Code { content, .. } => content,
            Block::Heading { content, .. } => content,
            Block::ListItem { content, .. } => content,
            Block::Quote { content } => content,
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
fn context_chars_of(messages: &[TranscriptMessage]) -> usize {
    let mut chars = 0usize;
    for m in messages {
        match &m.kind {
            MessageKind::Text => chars += m.raw.len(),
            MessageKind::Notice { .. } => chars += m.raw.len(),
            MessageKind::Thinking { content, .. } => chars += content.len(),
            MessageKind::ToolStep {
                arguments,
                output,
                children,
                ..
            } => {
                chars += arguments.len();
                if let Some(o) = output {
                    chars += o.len();
                }
                chars += context_chars_of(children);
            }
        }
    }
    chars
}

/// Rough token estimate for the active context, using the same ~4 chars/token
/// heuristic as `neenee_core`'s `estimate_string_tokens_len`.
pub fn estimate_context_tokens(messages: &[TranscriptMessage]) -> usize {
    context_chars_of(messages) / 4
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
#[derive(Debug, Clone)]
pub struct TranscriptMessage {
    pub role: Role,
    pub blocks: Vec<Block>,
    /// The original raw markdown/text, preserved for exact copy.
    pub raw: String,
    pub kind: MessageKind,
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
}

impl TranscriptMessage {
    pub fn new(role: Role, raw: impl Into<String>) -> Self {
        let raw = raw.into();
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
            role,
            blocks,
            raw,
            kind: MessageKind::Text,
            delivery: DeliveryStatus::default(),
            provider: None,
            model: None,
        }
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
            provider: None,
            model: None,
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
        // exit, an explicit `ToolOutput::Error`, a `failed` subagent). The
        // legacy `starts_with("Error")` text fallback was removed once tool
        // error sites migrated to `ToolOutput::Error` and sub-agents carried
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
                exit: None,
                truncated: false,
            }));
        }
        if let Some(neenee_core::ToolOutput::Shell { stdout, stderr, .. }) =
            structured.as_deref_mut()
        {
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
    /// untouched and returns `false`. When the step is a `task` (subagent),
    /// its still-running nested tool children are cancelled too, so an aborted
    /// subagent never leaves a "running" child step behind.
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
    /// (used for subagent children and as a defensive sweep). Returns `true`
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

    /// Append a subagent event as a nested child of this tool step.
    ///
    /// Returns `true` if this message is a tool step and the event was stored.
    pub fn push_subagent_event(&mut self, event: &SubagentEvent) -> bool {
        let MessageKind::ToolStep {
            children, profile, ..
        } = &mut self.kind
        else {
            return false;
        };
        match event {
            // The subagent announced its role — stamp it on the step so the
            // label can render "explore: …" / "plan: …" instead of a generic
            // "Subagent". No child message is produced.
            SubagentEvent::Started { profile: name } => {
                *profile = Some(name.to_string());
            }
            SubagentEvent::StreamStart => {
                children.push(TranscriptMessage::new(Role::Assistant, ""));
            }
            SubagentEvent::StreamDelta(delta) => {
                if let Some(last) = children
                    .last_mut()
                    .filter(|m| m.role == Role::Assistant && matches!(m.kind, MessageKind::Text))
                {
                    last.push_stream(delta);
                } else {
                    let mut msg = TranscriptMessage::new(Role::Assistant, "");
                    msg.push_stream(delta);
                    children.push(msg);
                }
            }
            SubagentEvent::StreamEnd(content) => {
                if let Some(last) = children.last_mut().filter(|m| m.role == Role::Assistant) {
                    last.raw = content.clone();
                    last.reparse();
                } else {
                    children.push(TranscriptMessage::new(Role::Assistant, content.clone()));
                }
            }
            SubagentEvent::ToolCall {
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
            SubagentEvent::ToolResult {
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
            SubagentEvent::Activity(_) => {}
            // Full-duplex (ADR-0029): a subagent surfaced a permission /
            // ask_user request up through the subagent tool. The down-direction
            // reply (registry → handle → reply_permission / reply_user_question)
            // is wired at the agent layer; rendering the nested prompt in the
            // TUI and routing the user's answer back down is the harness↔TUI
            // integration step that follows. Until then these are observed but
            // not rendered as a nested child step (the request still reaches
            // the harness via the `TurnEvent::SubAgent` envelope, so a future
            // handler can attach without changing the event shape).
            SubagentEvent::PermissionRequest(_) | SubagentEvent::UserQuestionRequest(_) => {}
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

    /// The `subagent` tool spawns a subagent. Such tool steps are rendered as a
    /// compact, non-expandable step that navigates into a dedicated subagent
    /// view on activation (see the TUI focus stack) rather than expanding
    /// inline.
    pub fn is_subagent_task(&self) -> bool {
        matches!(&self.kind, MessageKind::ToolStep { name, .. } if name == "subagent")
    }

    /// The call id of a tool step, used as the addressable identity of a
    /// subagent task for the focus stack.
    pub fn tool_step_call_id(&self) -> Option<&str> {
        match &self.kind {
            MessageKind::ToolStep { id, .. } => Some(id),
            _ => None,
        }
    }

    /// The nested child messages emitted by a subagent task. Returns `None`
    /// for non-tool-step messages.
    pub fn subagent_children(&self) -> Option<&[TranscriptMessage]> {
        match &self.kind {
            MessageKind::ToolStep { children, .. } => Some(children),
            _ => None,
        }
    }

    /// Mutable access to a tool step's child messages (used when the view is
    /// zoomed into a subagent and its children are the active message stream).
    pub fn subagent_children_mut(&mut self) -> Option<&mut Vec<TranscriptMessage>> {
        match &mut self.kind {
            MessageKind::ToolStep { children, .. } => Some(children),
            _ => None,
        }
    }

    /// Short label for the subagent, shown in the subagent view's navigation
    /// bar. Prefixed with the role (`explore` / `plan` / `verify` / …) when
    /// the `Started` event has identified it, so the bar reads e.g.
    /// `plan · write the implementation plan` rather than a bare description.
    pub fn subagent_label(&self) -> String {
        let MessageKind::ToolStep {
            arguments, profile, ..
        } = &self.kind
        else {
            return "Subagent".to_string();
        };
        let label = parse_arguments_kv(arguments)
            .into_iter()
            .find(|(k, _)| k == "description")
            .map(|(_, v)| v)
            .unwrap_or_else(|| "Subagent".to_string());
        let label = truncate(&label, 48);
        match profile {
            Some(role) => format!("{} · {}", role, label),
            None => label,
        }
    }

    /// One-line live status derived from the subagent's children and the
    /// parent tool step's completion state, e.g. `↳ Running · 3 tool calls ·
    /// Grep "foo"` or `↳ Completed · 3 tool calls · 1.2s`. Returns
    /// `None` for non-task steps. Duration is only shown once the step reaches
    /// a terminal state; a running step surfaces progress instead of an
    /// accumulating timer.
    pub fn subagent_status_line(&self) -> Option<String> {
        if !self.is_subagent_task() {
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
            provider: None,
            model: None,
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
            role: Role::System,
            blocks,
            raw,
            kind: MessageKind::Notice { severity },
            delivery: DeliveryStatus::default(),
            provider: None,
            model: None,
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
        let summary = crate::tui::render::tools::summary_for(name, arguments, profile.as_deref());
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
            }];
            if let Some(out) = output {
                self.raw.push_str("\n\n");
                self.raw.push_str(out);
                blocks.push(Block::Text {
                    content: out.clone(),
                });
            }
            self.blocks = blocks;
        } else {
            let summary =
                crate::tui::render::tools::summary_for(name, arguments, profile.as_deref());
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
        self.raw.push_str(delta);
        self.reparse();
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
    }]
}

fn parse_blocks_markdown(text: &str) -> Vec<Block> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let mut blocks = Vec::new();
    let mut paragraph = String::new();
    let mut heading: Option<(u8, String)> = None;
    let mut code_lang: Option<String> = None;
    let mut code_content = String::new();
    let mut in_code = false;
    let mut quotes = Vec::<String>::new();
    let mut lists = Vec::<ListState>::new();
    let mut items = Vec::<ListAccumulator>::new();
    let mut table = None::<TableAccumulator>;

    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => paragraph.clear(),
                Tag::Heading { level, .. } => {
                    heading = Some((heading_level(level), String::new()));
                }
                Tag::CodeBlock(lang) => {
                    in_code = true;
                    code_lang = match &lang {
                        pulldown_cmark::CodeBlockKind::Fenced(l) => Some(l.to_string()),
                        _ => None,
                    };
                    code_content.clear();
                }
                Tag::BlockQuote(_) => {
                    quotes.push(String::new());
                }
                Tag::List(start) => lists.push(ListState { next: start }),
                Tag::Item => {
                    let ordered = lists.last_mut().and_then(|list| {
                        let current = list.next?;
                        list.next = Some(current + 1);
                        Some(current)
                    });
                    items.push(ListAccumulator {
                        content: String::new(),
                        ordered,
                        depth: lists.len().saturating_sub(1),
                        checked: None,
                    });
                }
                Tag::Table(aligns) => {
                    table = Some(TableAccumulator {
                        aligns: aligns.into_iter().map(table_alignment).collect(),
                        ..TableAccumulator::default()
                    })
                }
                Tag::TableHead => {
                    if let Some(table) = &mut table {
                        table.in_head = true;
                        table.start_row();
                    }
                }
                Tag::TableRow => {
                    if let Some(table) = &mut table {
                        table.in_head = false;
                        table.start_row();
                    }
                }
                Tag::TableCell => {
                    if let Some(table) = &mut table {
                        table.start_cell();
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if items.is_empty() && quotes.is_empty() && table.is_none() {
                        push_block(
                            &mut blocks,
                            Block::Text {
                                content: paragraph.trim_end().to_string(),
                            },
                        );
                    }
                    paragraph.clear();
                }
                TagEnd::Heading(_) => {
                    if let Some((level, content)) = heading.take() {
                        push_block(
                            &mut blocks,
                            Block::Heading {
                                level,
                                content: content.trim_end().to_string(),
                            },
                        );
                    }
                }
                TagEnd::CodeBlock => {
                    in_code = false;
                    let content = code_content
                        .strip_prefix('\n')
                        .unwrap_or(&code_content)
                        .trim_end_matches('\n');
                    push_block(
                        &mut blocks,
                        Block::Code {
                            language: code_lang.take(),
                            content: content.to_string(),
                        },
                    );
                }
                TagEnd::BlockQuote(_) => {
                    if let Some(content) = quotes.pop() {
                        push_block(
                            &mut blocks,
                            Block::Quote {
                                content: content.trim_end().to_string(),
                            },
                        );
                    }
                }
                TagEnd::Item => {
                    if let Some(item) = items.pop() {
                        push_block(
                            &mut blocks,
                            Block::ListItem {
                                content: item.content.trim_end().to_string(),
                                ordered: item.ordered,
                                depth: item.depth,
                                checked: item.checked,
                            },
                        );
                    }
                }
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::TableCell => {
                    if let Some(table) = &mut table {
                        table.end_cell();
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(table) = &mut table {
                        table.end_row();
                    }
                }
                TagEnd::Table => {
                    if let Some(table) = table.take() {
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
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_code {
                    code_content.push_str(&t);
                } else {
                    append_text(
                        &t,
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::Code(t) => {
                if in_code {
                    code_content.push('`');
                    code_content.push_str(&t);
                    code_content.push('`');
                } else {
                    append_text(
                        &t,
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                if in_code {
                    code_content.push_str(&h);
                } else {
                    append_text(
                        &h,
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::SoftBreak => {
                if in_code {
                    code_content.push('\n');
                } else {
                    append_text(
                        " ",
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::HardBreak => {
                if in_code {
                    code_content.push('\n');
                } else {
                    append_text(
                        "\n",
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::Rule => {
                push_block(&mut blocks, Block::Rule);
            }
            Event::TaskListMarker(checked) => {
                if let Some(item) = items.last_mut() {
                    item.checked = Some(checked);
                }
            }
            _ => {}
        }
    }

    if !paragraph.trim().is_empty() {
        push_block(
            &mut blocks,
            Block::Text {
                content: paragraph.trim_end().to_string(),
            },
        );
    }
    while matches!(blocks.last(), Some(Block::Break)) {
        blocks.pop();
    }
    blocks
}

#[derive(Default)]
struct TableAccumulator {
    aligns: Vec<TableAlignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    in_head: bool,
}

impl TableAccumulator {
    fn start_row(&mut self) {
        self.row.clear();
    }

    fn end_row(&mut self) {
        if !self.cell.is_empty() {
            self.end_cell();
        }
        if self.row.is_empty() {
            return;
        }
        let row = std::mem::take(&mut self.row);
        if self.in_head {
            self.header = row;
        } else {
            self.rows.push(row);
        }
    }

    fn start_cell(&mut self) {
        self.cell.clear();
    }

    fn end_cell(&mut self) {
        self.row.push(std::mem::take(&mut self.cell));
    }

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
        let mut widths = vec![0usize; ncols];
        for (i, h) in self.header.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(width(h));
        }
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(ncols) {
                widths[i] = widths[i].max(width(cell));
            }
        }

        // Pad missing body cells up to the column count so the grid stays rectangular.
        let body_rows: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|row| {
                let mut padded = row.clone();
                if padded.len() < ncols {
                    padded.resize(ncols, String::new());
                }
                padded
            })
            .collect();

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
        for row in &body_rows {
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

fn table_alignment(a: pulldown_cmark::Alignment) -> TableAlignment {
    match a {
        pulldown_cmark::Alignment::None => TableAlignment::None,
        pulldown_cmark::Alignment::Left => TableAlignment::Left,
        pulldown_cmark::Alignment::Center => TableAlignment::Center,
        pulldown_cmark::Alignment::Right => TableAlignment::Right,
    }
}

fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

struct ListAccumulator {
    content: String,
    ordered: Option<u64>,
    depth: usize,
    checked: Option<bool>,
}

struct ListState {
    next: Option<u64>,
}

fn append_text(
    text: &str,
    heading: &mut Option<(u8, String)>,
    items: &mut [ListAccumulator],
    quotes: &mut [String],
    table: &mut Option<TableAccumulator>,
    paragraph: &mut String,
) {
    if let Some(table) = table {
        table.cell.push_str(text);
    } else if let Some((_, content)) = heading {
        content.push_str(text);
    } else if let Some(item) = items.last_mut() {
        item.content.push_str(text);
    } else if let Some(quote) = quotes.last_mut() {
        quote.push_str(text);
    } else {
        paragraph.push_str(text);
    }
}

fn push_block(blocks: &mut Vec<Block>, block: Block) {
    if block.is_empty() && !matches!(block, Block::Rule | Block::Break) {
        return;
    }
    let needs_gap = blocks.last().is_some_and(|previous| {
        !matches!(
            (previous, &block),
            (Block::Break, _)
                | (Block::Heading { .. }, Block::Text { .. })
                | (Block::ListItem { .. }, Block::ListItem { .. })
        )
    });
    if needs_gap {
        blocks.push(Block::Break);
    }
    blocks.push(block);
}

fn heading_level(level: pulldown_cmark::HeadingLevel) -> u8 {
    match level {
        pulldown_cmark::HeadingLevel::H1 => 1,
        pulldown_cmark::HeadingLevel::H2 => 2,
        pulldown_cmark::HeadingLevel::H3 => 3,
        pulldown_cmark::HeadingLevel::H4 => 4,
        pulldown_cmark::HeadingLevel::H5 => 5,
        pulldown_cmark::HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_text() {
        let blocks = parse_blocks("Hello world");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], Block::Text { content } if content == "Hello world"));
    }

    #[test]
    fn test_parse_code_block() {
        let text = "Some text\n\n```rust\nfn main() {}\n```\n\nMore text";
        let blocks = parse_blocks(text);
        assert_eq!(blocks.len(), 5);
        assert!(matches!(&blocks[0], Block::Text { content } if content == "Some text"));
        assert!(
            matches!(&blocks[2], Block::Code { language, content } if language.as_deref() == Some("rust") && content == "fn main() {}")
        );
        assert!(matches!(&blocks[4], Block::Text { content } if content == "More text"));
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
            Block::Heading { level: 1, content } if content == "Result"
        ));
        assert!(blocks.iter().any(|block| matches!(block, Block::Break)));
        assert!(blocks.iter().any(
            |block| matches!(block, Block::Text { content } if content == "First paragraph.")
        ));
        assert!(blocks.iter().any(
            |block| matches!(block, Block::Text { content } if content == "Second paragraph.")
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
                .any(|block| matches!(block, Block::Quote { content } if content == "quoted"))
        );
    }

    #[test]
    fn markdown_soft_breaks_flow_but_hard_breaks_are_preserved() {
        let soft = parse_blocks("第一行\n第二行");
        assert!(matches!(
            &soft[0],
            Block::Text { content } if content == "第一行 第二行"
        ));

        let hard = parse_blocks("第一行  \n第二行");
        assert!(matches!(
            &hard[0],
            Block::Text { content } if content == "第一行\n第二行"
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

    #[test]
    fn tool_step_collapses_and_restores_full_semantic_detail() {
        let mut message =
            TranscriptMessage::tool_step("call_1", "read_file", r#"{"path":"README.md"}"#);
        // Collapsed running: human-readable summary only — no tool name.
        assert!(message.raw.contains("Read README.md"));
        assert!(!message.raw.contains("read_file"));

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
    fn subagent_task_is_detected_and_addressable() {
        let task = TranscriptMessage::tool_step(
            "call_42",
            "subagent",
            r#"{"description":"explore src","prompt":"..."}"#,
        );
        assert!(task.is_subagent_task());
        assert_eq!(task.tool_step_call_id(), Some("call_42"));
        assert_eq!(task.subagent_children().map(|c| c.len()), Some(0));
        assert_eq!(task.subagent_label(), "explore src");

        // A regular tool step is not a subagent task.
        let read = TranscriptMessage::tool_step("call_1", "read_file", r#"{"path":"a"}"#);
        assert!(!read.is_subagent_task());
        assert!(read.subagent_status_line().is_none());
    }

    #[test]
    fn subagent_started_event_labels_step_by_role() {
        // A `Started` event stamps the bound profile name on the step so the
        // nav bar / collapsed summary read by role (`plan · …`) instead of a
        // generic "Subagent".
        let mut task = TranscriptMessage::tool_step(
            "call_7",
            "subagent",
            r#"{"description":"write the plan","prompt":"..."}"#,
        );
        assert_eq!(task.subagent_label(), "write the plan");
        assert!(
            task.push_subagent_event(&neenee_core::SubagentEvent::Started { profile: "explore" })
        );
        assert_eq!(task.subagent_label(), "explore · write the plan");
        // The collapsed header picks the role up via `tool_step_summary` too.
        let header = task.tool_step_summary().expect("summary");
        assert!(
            header.starts_with("explore:"),
            "collapsed summary should lead with the role; got: {header}"
        );
    }

    #[test]
    fn subagent_status_reflects_children_and_completion() {
        let mut task = TranscriptMessage::tool_step(
            "call_9",
            "subagent",
            r#"{"description":"d","prompt":"p"}"#,
        );

        // No children yet, still running.
        let running = task.subagent_status_line().expect("running status");
        assert!(running.starts_with("↳ Running"), "got: {running}");

        // Streaming assistant text => a "thinking" suffix.
        task.push_subagent_event(&SubagentEvent::StreamStart);
        task.push_subagent_event(&SubagentEvent::StreamDelta("partial".into()));
        let thinking = task.subagent_status_line().expect("thinking status");
        assert!(thinking.starts_with("↳ Running"), "got: {thinking}");
        assert!(thinking.ends_with("thinking"), "got: {thinking}");

        // An in-flight child tool call surfaces the tool's header.
        task.push_subagent_event(&SubagentEvent::ToolCall {
            id: "inner".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"foo"}"#.into(),
        });
        let running = task.subagent_status_line().expect("running status");
        assert!(running.starts_with("↳ Running"), "got: {running}");
        assert!(running.contains("Grep"), "got: {running}");

        // Completing the parent summarizes tool-call count + duration.
        assert!(task.finish_tool_step(
            "call_9",
            "final answer",
            neenee_core::ToolOutput::text("final answer"),
            1500
        ));
        let done = task.subagent_status_line().expect("done status");
        assert!(done.starts_with("↳ Completed"), "got: {done}");
        assert!(done.contains("1 tool calls"), "got: {done}");
        assert!(done.contains("1.5s"), "got: {done}");

        // Children are accessible for the dedicated subagent view.
        assert_eq!(task.subagent_children().map(|c| c.len()), Some(2));
    }

    #[test]
    fn subagent_failed_status_reports_failure() {
        let mut task =
            TranscriptMessage::tool_step("c", "subagent", r#"{"description":"d","prompt":"p"}"#);
        task.push_subagent_event(&SubagentEvent::ToolCall {
            id: "i".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        // The subagent failure is now signalled by the structured `failed`
        // flag on `ToolOutput::Subagent`, not by an "Error:" text prefix.
        let structured = neenee_core::ToolOutput::Subagent {
            summary: "Error: boom".into(),
            messages: Vec::new(),
            usage: neenee_core::TokenUsage::default(),
            failed: true,
        };
        assert!(task.finish_tool_step("c", structured.to_text(), structured, 100));
        let status = task.subagent_status_line().unwrap();
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
            exit: Some(1),
            truncated: false,
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
            exit: Some(0),
            truncated: false,
        };
        let text = structured.to_text();
        assert!(step.finish_tool_step("c", text, structured, 5));
        assert_eq!(step.tool_step_status(), Some(ToolStepStatus::Ok));
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
    fn cancelling_a_subagent_also_cancels_its_running_children() {
        let mut task = TranscriptMessage::tool_step(
            "task_1",
            "subagent",
            r#"{"description":"d","prompt":"p"}"#,
        );
        // A nested tool call still in flight.
        task.push_subagent_event(&SubagentEvent::ToolCall {
            id: "inner".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"foo"}"#.into(),
        });
        let children = task.subagent_children().expect("has children");
        assert_eq!(
            children[0].tool_step_status(),
            Some(ToolStepStatus::Running)
        );

        // Interrupting the parent task cancels it AND the nested running child,
        // so the subagent view never shows a stuck "running" step.
        assert!(task.cancel_tool_step("task_1"));
        assert_eq!(task.tool_step_status(), Some(ToolStepStatus::Cancelled));
        let children = task.subagent_children().expect("has children");
        assert_eq!(
            children[0].tool_step_status(),
            Some(ToolStepStatus::Cancelled),
            "nested child must converge with the parent"
        );

        let status = task.subagent_status_line().expect("status line");
        assert!(status.starts_with("↳ Cancelled"), "got: {status}");
    }

    #[test]
    fn cancel_all_running_is_a_defensive_sweep_that_skips_terminal_steps() {
        let mut a = TranscriptMessage::tool_step("a", "read_file", "{}");
        let mut b = TranscriptMessage::tool_step("b", "read_file", "{}");
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
}
