use tonic::{transport::Server, Request, Response, Status};
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use neenee_core::{Agent, AgentMode, Message, Role, providers::MockProvider, tools::{BashTool, FileReadTool, FileWriteTool, GrepTool, GlobTool}};

pub mod agent {
    tonic::include_proto!("agent");
}

use agent::agent_service_server::{AgentService, AgentServiceServer};
use agent::{ChatRequest, ChatResponse, StreamChatResponse};

pub struct MyAgentService {
    agent: Arc<Agent>,
    history: Arc<tokio::sync::Mutex<Vec<Message>>>,
}

#[tonic::async_trait]
impl AgentService for MyAgentService {
    async fn chat(&self, request: Request<ChatRequest>) -> Result<Response<ChatResponse>, Status> {
        let req = request.into_inner();
        let mut history = self.history.lock().await;
        
        history.push(Message {
            role: Role::User,
            content: req.message,
            tool_calls: None,
        });

        match self.agent.run(&mut history).await {
            Ok(response) => Ok(Response::new(ChatResponse {
                response: response.content,
            })),
            Err(err) => Err(Status::internal(err)),
        }
    }

    type StreamChatStream = tokio_stream::wrappers::ReceiverStream<Result<StreamChatResponse, Status>>;

    async fn stream_chat(&self, _request: Request<ChatRequest>) -> Result<Response<Self::StreamChatStream>, Status> {
        Err(Status::unimplemented("Streaming not yet implemented"))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .init();

    let provider = Arc::new(MockProvider);
    let tools = vec![
        Arc::new(BashTool) as Arc<dyn neenee_core::Tool>,
        Arc::new(FileReadTool) as Arc<dyn neenee_core::Tool>,
        Arc::new(FileWriteTool) as Arc<dyn neenee_core::Tool>,
        Arc::new(GrepTool) as Arc<dyn neenee_core::Tool>,
        Arc::new(GlobTool) as Arc<dyn neenee_core::Tool>,
    ];
    let agent = Arc::new(Agent::new(provider, tools, AgentMode::Build));
    let history = Arc::new(tokio::sync::Mutex::new(vec![
        Message {
            role: Role::System,
            content: "You are neenee, a helpful AI coding assistant.".to_string(),
            tool_calls: None,
        }
    ]));

    let addr = "127.0.0.1:50051".parse()?;
    let service = MyAgentService { agent, history };

    tracing::info!("AgentService listening on {}", addr);

    Server::builder()
        .add_service(AgentServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
