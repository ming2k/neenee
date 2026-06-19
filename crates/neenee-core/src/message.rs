//! Conversation message types shared across the harness, providers, and UI.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Content-addressed storage hash for large payloads. When present the
    /// inline `content` may be empty on disk and is rehydrated from the blob
    /// store on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Optional tool calls attached to an assistant message. Marked
    /// `#[serde(default)]` so hand-written or stripped JSON messages (e.g. test
    /// fixtures, externally generated snapshots) can omit the key entirely
    /// instead of having to spell out `"tool_calls": null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Inline image attachments (typically pasted into the prompt). Each part
    /// carries a MIME type and already-base64-encoded bytes so it can be
    /// emitted directly as an OpenAI `image_url` data URL or a Gemini
    /// `inline_data` part. Only user messages normally carry images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImagePart>>,
    /// Identifier of the provider/solution that produced this assistant
    /// message (e.g. `"kimi-code"`, `"gemini"`). Stamped by the harness so a
    /// session that mixes multiple models stays traceable after resume. Other
    /// roles leave this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model identifier that produced this assistant message (e.g.
    /// `"kimi-for-coding"`). Companion to [`Message::provider`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    /// Nested sub-agent transcript. Populated only on the `Tool`-role result
    /// message of a `task` tool call (see `TaskTool`). Each entry is a
    /// `Message` from the sub-agent's own conversation (System, User,
    /// Assistant with tool_calls, Tool results, …), in chronological order.
    /// Recursive: a sub-agent's own `task` results carry their own `children`,
    /// so arbitrarily deep sub-agent trees round-trip through session.json.
    ///
    /// `None` for every message that is not a sub-agent's tool result; this
    /// keeps the legacy flat shape unchanged for non-task messages and lets
    /// old session.json files (which predate the field) deserialize as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Message>>,
    /// Metadata about the sub-agent run that produced [`Message::children`].
    /// Populated only on the same message that has `children = Some(_)`. The
    /// two fields are convention-paired (presence of one implies presence of
    /// the other); they are kept separate rather than bundled into a single
    /// `subagent: Option<Payload>` field so the schema stays backward-
    /// compatible without a custom deserializer — old session.json files
    /// simply have `subagent_meta = None` and `children = Some(...)`, and the
    /// harness fills in best-effort defaults on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_meta: Option<SubagentMeta>,
}

/// Sidecar metadata for a sub-agent run. Lives next to
/// [`Message::children`] on the same `Tool`-role result message. Captures
/// information that the live event stream knows but the bare transcript
/// cannot reconstruct on resume.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubagentMeta {
    /// The task description supplied by the parent agent (from the `task`
    /// tool_call's `arguments.description` field). Cached here so the TUI
    /// does not have to re-parse the JSON arguments to label the sub-agent
    /// view's navigation bar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Wall-clock duration of the sub-agent run in milliseconds. Filled from
    /// the parent `record_tool_result`'s `duration_ms` parameter (which
    /// already measures the full sub-agent run because the `task` tool blocks
    /// until the sub-agent finishes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Number of read-only tools the sub-agent had access to. Useful as a
    /// debugging signal when reviewing archived runs.
    #[serde(default)]
    pub toolset_count: u32,
    /// Provider / model that served the sub-agent. Currently always equal to
    /// the parent's provider/model (TaskTool clones the parent's provider),
    /// but persisted separately so a future "cheaper model for sub-agents"
    /// feature does not require a schema change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether the sub-agent finished by hitting an error path (32-round
    /// limit, repeated-call guard, provider error). Mirrors
    /// `ToolOutput::Subagent { summary.starts_with("Error") }` but stored
    /// explicitly so consumers do not have to string-sniff.
    #[serde(default)]
    pub failed: bool,
}

/// An inline image attached to a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImagePart {
    /// MIME type, e.g. `"image/png"`.
    pub mime: String,
    /// Base64-encoded image bytes.
    pub data: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            content_blob: None,
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            subagent_meta: None,
        }
    }

    pub fn hidden(role: Role, content: impl Into<String>) -> Self {
        let mut message = Self::new(role, content);
        message.hidden = true;
        message
    }

    pub fn with_display_content(mut self, content: impl Into<String>) -> Self {
        self.display_content = Some(content.into());
        self
    }

    pub fn with_images(mut self, images: Vec<ImagePart>) -> Self {
        self.images = if images.is_empty() {
            None
        } else {
            Some(images)
        };
        self
    }

    /// Stamp the provider/solution id and model that produced this message,
    /// so the transcript stays traceable when a session spans multiple models.
    pub fn with_attribution(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.provider = Some(provider.into());
        self.model = Some(model.into());
        self
    }

    pub fn tool_result(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            content_blob: None,
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(call.id.clone()),
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            subagent_meta: None,
        }
    }

    /// Attach a sub-agent's full internal transcript to a `Tool`-role result
    /// message. Builder-style companion to [`Message::tool_result`]. Storing
    /// the nested transcript on the result message (rather than on the
    /// assistant `tool_calls` message) keeps the data close to where it was
    /// produced and lets resume reconstruct the sub-agent view by reading a
    /// single message.
    pub fn with_children(mut self, children: Vec<Message>) -> Self {
        self.children = if children.is_empty() {
            None
        } else {
            Some(children)
        };
        self
    }

    /// Attach sub-agent sidecar metadata to a `Tool`-role result message.
    /// Pair with [`Message::with_children`]; the two fields travel together
    /// but are kept separate for schema-backward-compat (see
    /// [`Message::subagent_meta`] docs).
    pub fn with_subagent_meta(mut self, meta: SubagentMeta) -> Self {
        self.subagent_meta = Some(meta);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_without_children_omits_field_in_json() {
        // Legacy compatibility: a normal Message must still serialise without
        // a `children` key so old consumers / tests that match the literal
        // JSON keep working.
        let m = Message::new(Role::User, "hi");
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("children"), "json should omit children: {json}");
    }

    #[test]
    fn legacy_json_without_children_deserialises_to_none() {
        // Pre-Phase-3 snapshots must load unchanged.
        let json = r#"{"role":"User","content":"hi","hidden":false}"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert_eq!(m.content, "hi");
        assert!(m.children.is_none());
    }

    #[test]
    fn children_round_trip_through_json() {
        // A tool result with a sub-agent transcript must survive a
        // serialise → deserialise round trip with the nested messages intact,
        // including their own nested children (sub-sub-agents).
        let call = ToolCall {
            id: "call_root".to_string(),
            name: "task".to_string(),
            arguments: "{}".to_string(),
        };
        let nested_call = ToolCall {
            id: "call_inner".to_string(),
            name: "grep".to_string(),
            arguments: r#"{"pattern":"foo"}"#.to_string(),
        };
        let inner_child = Message::new(Role::Tool, "match at a.rs:1").with_children(vec![
            Message::new(Role::Assistant, "deeply nested note"),
        ]);
        let subagent_transcript = vec![
            Message::new(Role::System, "subagent system"),
            Message::new(Role::User, "subagent task"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                content_blob: None,
                display_content: None,
                reasoning_content: None,
                tool_calls: Some(vec![nested_call]),
                tool_call_id: None,
                images: None,
                provider: None,
                model: None,
                hidden: false,
                children: None,
                subagent_meta: None,
            },
            inner_child,
        ];
        let parent = Message::tool_result(&call, "[task result]:\nfound it")
            .with_children(subagent_transcript);

        let json = serde_json::to_string_pretty(&parent).unwrap();
        let restored: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.role, Role::Tool);
        assert_eq!(restored.tool_call_id.as_deref(), Some("call_root"));
        let children = restored.children.expect("children round-trip");
        assert_eq!(children.len(), 4);
        // The grep call inside the subagent kept its tool_calls.
        assert!(children[2].tool_calls.is_some());
        // The inner Tool message kept its own nested children (sub-sub-agent).
        let inner = &children[3];
        assert_eq!(inner.role, Role::Tool);
        assert!(inner.children.is_some(), "sub-sub-agent children must survive");
        assert_eq!(inner.children.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn with_children_empty_vec_is_none() {
        let call = ToolCall {
            id: "c".to_string(),
            name: "task".to_string(),
            arguments: "{}".to_string(),
        };
        let m = Message::tool_result(&call, "x").with_children(Vec::new());
        assert!(m.children.is_none(), "empty children should collapse to None");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
}
