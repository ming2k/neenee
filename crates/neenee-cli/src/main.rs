use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use neenee_core::{Agent, AgentMode, Message, Role, AgentRequest, AgentResponse, Provider, async_trait, providers::{MockProvider, OpenAIProvider, GeminiProvider, LlamaServerProvider}, tools::{BashTool, FileReadTool, FileWriteTool, GrepTool, GlobTool}};
use neenee_tui::start_tui;
use futures::StreamExt;

struct ProxyProvider {
    holder: Arc<RwLock<Arc<dyn Provider>>>,
}

#[async_trait]
impl Provider for ProxyProvider {
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
        let p = self.holder.read().await;
        p.chat(messages).await
    }
    async fn stream_chat(&self, messages: Vec<Message>) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String> {
        let p = self.holder.read().await;
        p.stream_chat(messages).await
    }
}

mod config;
use config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<AgentRequest>();
    let (resp_tx, resp_rx) = mpsc::unbounded_channel::<AgentResponse>();

    let mut config = Config::load();

    // Initialize Agent logic
    let initial_provider: Arc<dyn Provider> = match config.default_provider.as_str() {
        "llama" => Arc::new(LlamaServerProvider::new(
            std::env::var("LLAMA_BASE_URL").ok().or(config.llama_base_url.clone()).unwrap_or_else(|| "http://localhost:8080".to_string()),
            std::env::var("LLAMA_MODEL").ok().or(config.llama_model.clone()).unwrap_or_else(|| "local-model".to_string())
        )),
        "gemini" => Arc::new(GeminiProvider::new(
            std::env::var("GEMINI_API_KEY").ok().or(config.gemini_api_key.clone()).unwrap_or_default(),
            std::env::var("GEMINI_MODEL").ok().or(config.gemini_model.clone()).unwrap_or_else(|| "gemini-1.5-flash".to_string())
        )),
        "openai" => Arc::new(OpenAIProvider::new(
            std::env::var("OPENAI_API_KEY").ok().or(config.openai_api_key.clone()).unwrap_or_default(),
            std::env::var("OPENAI_MODEL").ok().or(config.openai_model.clone()).unwrap_or_else(|| "gpt-4o".to_string())
        )),
        _ => Arc::new(MockProvider),
    };

    let provider_holder = Arc::new(RwLock::new(initial_provider));
    let provider_for_task = provider_holder.clone();

    let agent_provider = Arc::new(ProxyProvider { holder: provider_holder });
    let tools = vec![
        Arc::new(BashTool) as Arc<dyn neenee_core::Tool>,
        Arc::new(FileReadTool) as Arc<dyn neenee_core::Tool>,
        Arc::new(FileWriteTool) as Arc<dyn neenee_core::Tool>,
        Arc::new(GrepTool) as Arc<dyn neenee_core::Tool>,
        Arc::new(GlobTool) as Arc<dyn neenee_core::Tool>,
    ];
    let agent = Arc::new(Agent::new(agent_provider, tools, AgentMode::Build));
    
    let mut history = vec![
        Message {
            role: Role::System,
            content: "You are neenee, a helpful AI coding assistant.".to_string(),
            tool_calls: None,
        }
    ];

    // Load history
    let input_history = Config::load_history();

    let current_task_token = Arc::new(RwLock::new(None::<CancellationToken>));
    let ctt_clone = current_task_token.clone();

    // Initial values for TUI
    let initial_p_name = config.default_provider.clone();
    let initial_m_name = match initial_p_name.as_str() {
        "openai" => config.openai_model.clone().unwrap_or_else(|| "gpt-4o".to_string()),
        "gemini" => config.gemini_model.clone().unwrap_or_else(|| "gemini-1.5-flash".to_string()),
        "llama" => config.llama_model.clone().unwrap_or_else(|| "local-model".to_string()),
        _ => "mock-model".to_string(),
    };

    // Spawn Agent Background Task
    tokio::spawn(async move {
        while let Some(req) = req_rx.recv().await {
            match req {
                AgentRequest::Interrupt => {
                    let mut token = ctt_clone.write().await;
                    if let Some(t) = token.take() {
                        t.cancel();
                    }
                }
                AgentRequest::SwitchProvider { provider_type, model, .. } => {
                    let new_p: Arc<dyn Provider> = match provider_type.as_str() {
                        "openai" => Arc::new(OpenAIProvider::new(
                            std::env::var("OPENAI_API_KEY").ok().or(config.openai_api_key.clone()).unwrap_or_default(),
                            model.clone()
                        )),
                        "gemini" => Arc::new(GeminiProvider::new(
                            std::env::var("GEMINI_API_KEY").ok().or(config.gemini_api_key.clone()).unwrap_or_default(),
                            model.clone()
                        )),
                        "llama" => Arc::new(LlamaServerProvider::new(
                            std::env::var("LLAMA_BASE_URL").ok().or(config.llama_base_url.clone()).unwrap_or_else(|| "http://localhost:8080".to_string()),
                            model.clone()
                        )),
                        _ => Arc::new(MockProvider),
                    };
                    *provider_for_task.write().await = new_p;
                    
                    // Update and save config
                    config.default_provider = provider_type.clone();
                    match provider_type.as_str() {
                        "openai" => config.openai_model = Some(model.clone()),
                        "gemini" => config.gemini_model = Some(model.clone()),
                        "llama" => config.llama_model = Some(model.clone()),
                        _ => {}
                    }
                    let _ = config.save();

                    let _ = resp_tx.send(AgentResponse::ProviderSwitched { 
                        provider: provider_type, 
                        model 
                    });
                }
                AgentRequest::SlashCommand(cmd) => {
                    let parts: Vec<&str> = cmd.split_whitespace().collect();
                    match parts[0] {
                        "/models" => {
                            // Handled in TUI
                        }
                        "/mode" => {
                            if parts.len() > 1 {
                                let _ = resp_tx.send(AgentResponse::Text(format!("Mode changed to: {}", parts[1])));
                            } else {
                                let _ = resp_tx.send(AgentResponse::Text("Current mode: Build".to_string()));
                            }
                        }
                        "/help" => {
                            let _ = resp_tx.send(AgentResponse::Text(
                                "Slash commands:\n/models - Select LLM provider\n/mode - Show/change mode\n/exit - Exit the program\n/help - Show this message".to_string()
                            ));
                        }
                        "/exit" => {
                            let _ = resp_tx.send(AgentResponse::Exit);
                        }
                        _ => {
                            let _ = resp_tx.send(AgentResponse::Error(format!("Unknown command: {}", parts[0])));
                        }
                    }
                }
                AgentRequest::Chat(text) => {
                    history.push(Message {
                        role: Role::User,
                        content: text,
                        tool_calls: None,
                    });

                    let token = CancellationToken::new();
                    *ctt_clone.write().await = Some(token.clone());

                    let agent_clone = agent.clone();
                    let mut history_clone = history.clone();
                    let resp_tx_clone = resp_tx.clone();
                    let ctt_inner = ctt_clone.clone();

                    tokio::spawn(async move {
                        let result = tokio::select! {
                            _ = token.cancelled() => {
                                let _ = resp_tx_clone.send(AgentResponse::Text("... [Interrupted]".to_string()));
                                return;
                            }
                            res = agent_clone.provider.stream_chat(history_clone.clone()) => res,
                        };

                        match result {
                            Ok(mut stream) => {
                                let _ = resp_tx_clone.send(AgentResponse::StreamStart);
                                let mut full_content = String::new();
                                loop {
                                    tokio::select! {
                                        _ = token.cancelled() => {
                                            let _ = resp_tx_clone.send(AgentResponse::StreamDelta(" [Interrupted]".to_string()));
                                            break;
                                        }
                                        chunk = stream.next() => {
                                            if let Some(c) = chunk {
                                                match c {
                                                    Ok(delta) => {
                                                        full_content.push_str(&delta);
                                                        let _ = resp_tx_clone.send(AgentResponse::StreamDelta(delta));
                                                    }
                                                    Err(e) => {
                                                        let _ = resp_tx_clone.send(AgentResponse::Error(e));
                                                        break;
                                                    }
                                                }
                                            } else {
                                                break;
                                            }
                                        }
                                    }
                                }
                                let _ = resp_tx_clone.send(AgentResponse::StreamEnd);
                                
                                // Note: In a real app we'd need to sync this back to the main history
                                // For now we just push it to our local copy which might get out of sync
                                // if we don't handle it carefully.
                            }
                            Err(_) => {
                                // Fallback to non-streaming
                                let res = tokio::select! {
                                    _ = token.cancelled() => return,
                                    res = agent_clone.run(&mut history_clone) => res,
                                };
                                match res {
                                    Ok(response) => {
                                        let _ = resp_tx_clone.send(AgentResponse::Text(response.content));
                                    }
                                    Err(err) => {
                                        let _ = resp_tx_clone.send(AgentResponse::Error(err));
                                    }
                                }
                            }
                        }
                        *ctt_inner.write().await = None;
                    });
                }
            }
        }
    });

    // Start TUI in the main thread
    match start_tui(req_tx, resp_rx, initial_p_name, initial_m_name, input_history).await {
        Ok(history) => {
            let _ = Config::save_history(&history);
        }
        Err(e) => return Err(e),
    }

    Ok(())
}
