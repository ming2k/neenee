//! Trivial mock provider used as the default channel and in tests.

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use neenee_core::{Message, Provider, Role};

pub struct MockProvider;

#[async_trait]
impl Provider for MockProvider {
    async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
        Ok(Message {
            role: Role::Assistant,
            content: "Hello! I am a mock AI. How can I help you today?".to_string(),
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
        })
    }

    fn provider_id(&self) -> String {
        "mock".to_string()
    }

    fn model(&self) -> String {
        "mock".to_string()
    }

    async fn stream_chat(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        let response = vec![
            Ok("This ".to_string()),
            Ok("is ".to_string()),
            Ok("a ".to_string()),
            Ok("streaming ".to_string()),
            Ok("mock ".to_string()),
            Ok("response ".to_string()),
            Ok("from ".to_string()),
            Ok("neenee!".to_string()),
        ];
        Ok(futures::stream::iter(response).boxed())
    }
}
