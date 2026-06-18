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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
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
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
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
            display_content: None,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(call.id.clone()),
            images: None,
            provider: None,
            model: None,
            hidden: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
}
