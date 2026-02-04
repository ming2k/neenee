use serde::{Deserialize, Serialize};
pub use async_trait::async_trait;
use futures::stream::BoxStream;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub tool_calls: Option<Vec<ToolCall>>,
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

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String>;
    async fn stream_chat(&self, messages: Vec<Message>) -> Result<BoxStream<'static, Result<String, String>>, String>;
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn call(&self, arguments: &str) -> Result<String, String>;
}

pub mod tools;
pub mod providers;

#[derive(Debug)]
pub enum AgentRequest {
    Chat(String),
    SlashCommand(String),
    Interrupt,
    SwitchProvider {
        provider_type: String,
        model: String,
        api_key: Option<String>,
        base_url: Option<String>,
    },
}

#[derive(Debug)]
pub enum AgentResponse {
    Text(String),
    StreamStart,
    StreamDelta(String),
    StreamEnd,
    Error(String),
    Exit,
    ProviderSwitched { provider: String, model: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    Build,
    Plan,
}

pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub mode: AgentMode,
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>, mode: AgentMode) -> Self {
        Self { provider, tools, mode }
    }

    pub async fn run(&self, messages: &mut Vec<Message>) -> Result<Message, String> {
        loop {
            let response = self.provider.chat(messages.clone()).await?;
            messages.push(response.clone());

            if let Some(tool_calls) = &response.tool_calls {
                if tool_calls.is_empty() {
                    return Ok(response);
                }

                for call in tool_calls {
                    if self.mode == AgentMode::Plan && (call.name == "write_file" || call.name == "bash") {
                        messages.push(Message {
                            role: Role::Tool,
                            content: format!("Skipped tool '{}' in Plan mode.", call.name),
                            tool_calls: None,
                        });
                        continue;
                    }

                    let tool = self.tools.iter().find(|t| t.name() == call.name)
                        .ok_or_else(|| format!("Tool not found: {}", call.name))?;
                    
                    match tool.call(&call.arguments).await {
                        Ok(output) => {
                            messages.push(Message {
                                role: Role::Tool,
                                content: output,
                                tool_calls: None,
                            });
                        }
                        Err(err) => {
                            messages.push(Message {
                                role: Role::Tool,
                                content: format!("Error: {}", err),
                                tool_calls: None,
                            });
                        }
                    }
                }
                // Continue the loop to let the provider see the tool results
            } else {
                return Ok(response);
            }
        }
    }
}