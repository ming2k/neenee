//! Minimal MCP client support for local stdio servers.
//!
//! Each configured server is initialized once at startup. Its advertised tools
//! are adapted to neenee's `Tool` trait and use the same agent execution path
//! as built-in tools.

use async_trait::async_trait;
use neenee_core::mcp::{McpConnectionStatus, McpServerConfig};
use neenee_core::{Tool, ToolAccess};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

pub struct McpLoadResult {
    pub tools: Vec<Arc<dyn Tool>>,
    pub statuses: Vec<(String, McpConnectionStatus)>,
}

struct McpTransport {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

struct McpClient {
    transport: Mutex<McpTransport>,
    next_id: AtomicU64,
}

impl McpClient {
    async fn connect(config: &McpServerConfig) -> Result<Arc<Self>, String> {
        let (program, args) = config
            .command
            .split_first()
            .ok_or_else(|| "MCP command must not be empty".to_string())?;

        let mut command = Command::new(program);
        command
            .args(args)
            .envs(&config.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to spawn '{}': {}", program, error))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "MCP server stdin is unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "MCP server stdout is unavailable".to_string())?;
        let client = Arc::new(Self {
            transport: Mutex::new(McpTransport {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            next_id: AtomicU64::new(1),
        });

        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "neenee",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut transport = self.transport.lock().await;
        write_message(&mut transport.stdin, &payload).await?;

        loop {
            let response = read_message(&mut transport.stdout).await?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(format!("MCP {} error: {}", method, error));
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| format!("MCP {} response has no result", method));
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut transport = self.transport.lock().await;
        write_message(&mut transport.stdin, &payload).await
    }

    async fn list_tools(
        self: &Arc<Self>,
        server: &str,
        read_only: bool,
    ) -> Result<Vec<Arc<dyn Tool>>, String> {
        let result = self.request("tools/list", json!({})).await?;
        let definitions = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| "MCP tools/list response has no tools array".to_string())?;

        definitions
            .iter()
            .map(|definition| {
                let original_name = definition
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "MCP tool has no name".to_string())?
                    .to_string();
                let public_name = format!(
                    "mcp__{}__{}",
                    sanitize_name(server),
                    sanitize_name(&original_name)
                );
                let description = definition
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("MCP tool")
                    .to_string();
                let parameters = definition
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"}));
                Ok(Arc::new(McpTool {
                    public_name,
                    original_name,
                    description,
                    parameters,
                    access: if read_only {
                        ToolAccess::Read
                    } else {
                        ToolAccess::Write
                    },
                    client: self.clone(),
                }) as Arc<dyn Tool>)
            })
            .collect()
    }
}

struct McpTool {
    public_name: String,
    original_name: String,
    description: String,
    parameters: Value,
    access: ToolAccess,
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.public_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn access(&self) -> ToolAccess {
        self.access
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let arguments: Value = serde_json::from_str(arguments)
            .map_err(|error| format!("invalid MCP tool arguments: {}", error))?;
        let result = self
            .client
            .request(
                "tools/call",
                json!({
                    "name": self.original_name,
                    "arguments": arguments,
                }),
            )
            .await?;
        Ok(render_tool_result(&result))
    }
}

pub async fn load_mcp_tools(configs: &HashMap<String, McpServerConfig>) -> McpLoadResult {
    let mut tools = Vec::new();
    let mut statuses = Vec::new();
    let mut names = configs.keys().cloned().collect::<Vec<_>>();
    names.sort();

    for name in names {
        let config = &configs[&name];
        if !config.enabled {
            statuses.push((name, McpConnectionStatus::Disabled));
            continue;
        }

        let loaded = timeout(MCP_CONNECT_TIMEOUT, async {
            let client = McpClient::connect(config).await?;
            client.list_tools(&name, config.read_only).await
        })
        .await;

        match loaded {
            Ok(Ok(server_tools)) => {
                let count = server_tools.len();
                tools.extend(server_tools);
                statuses.push((name, McpConnectionStatus::Connected { tools: count }));
            }
            Ok(Err(error)) => statuses.push((name, McpConnectionStatus::Failed(error))),
            Err(_) => statuses.push((
                name,
                McpConnectionStatus::Failed("connection timed out".to_string()),
            )),
        }
    }

    McpLoadResult { tools, statuses }
}

async fn write_message(stdin: &mut ChildStdin, message: &Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stdin
        .write_all(&line)
        .await
        .map_err(|error| error.to_string())?;
    stdin.flush().await.map_err(|error| error.to_string())
}

async fn read_message(stdout: &mut BufReader<ChildStdout>) -> Result<Value, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = stdout
            .read_line(&mut line)
            .await
            .map_err(|error| error.to_string())?;
        if bytes == 0 {
            return Err("MCP server closed stdout".to_string());
        }
        if line.trim().is_empty() {
            continue;
        }
        return serde_json::from_str(line.trim())
            .map_err(|error| format!("invalid MCP JSON-RPC message: {}", error));
    }
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn render_tool_result(result: &Value) -> String {
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let rendered = content
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !rendered.is_empty() {
            return rendered;
        }
    }
    result.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_for_provider_function_schemas() {
        assert_eq!(sanitize_name("git tools/read-file"), "git_tools_read_file");
    }

    #[test]
    fn text_content_is_rendered_without_protocol_wrappers() {
        let result = json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ]
        });
        assert_eq!(render_tool_result(&result), "first\nsecond");
    }
}
