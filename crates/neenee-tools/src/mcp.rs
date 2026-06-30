//! Minimal MCP client support for local stdio servers.
//!
//! Each configured server is initialized once at startup. Its advertised tools
//! are adapted to neenee's `Tool` trait and use the same agent execution path
//! as built-in tools.
//!
//! # Error model
//!
//! `McpError` separates **transport** failures (the stdio pipe broke — the
//! server crashed or the child process died) from **protocol** failures (the
//! server replied with a JSON-RPC `error` object, or a well-formed but useless
//! response). This distinction is load-bearing for retry safety: only a
//! transport error justifies a reconnect-and-retry, because a protocol error is
//! a deterministic, server-side result that re-sending the same call would
//! reproduce identically — and for a *non-idempotent* MCP tool re-sending it
//! would repeat a side effect.

use async_trait::async_trait;
use neenee_core::Tool;
use neenee_core::mcp::{McpConnectionStatus, McpServerConfig};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// Per-call timeout for a single JSON-RPC request over an established
/// connection. A server that accepts the request but never responds (or streams
/// nothing forever) is released instead of pinning the agent indefinitely.
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// A typed MCP error: either a broken transport (retry-safe) or a protocol
/// result (never retry a protocol error — re-sending an already-applied
/// non-idempotent call would double a side effect).
#[derive(Debug)]
enum McpError {
    /// The stdio pipe broke: a failed write/read, the child exited, or a
    /// request timed out. The connection is unusable and a reconnect may help.
    Transport(String),
    /// The server replied with a JSON-RPC `error` object (or a malformed
    /// response). This is a deterministic server-side result — retrying the
    /// *same* call yields the *same* outcome, so never reconnect on it.
    Protocol(String),
}

impl McpError {
    /// True only for transport-level failures, where reconnecting and retrying
    /// once could plausibly succeed.
    fn is_transport(&self) -> bool {
        matches!(self, McpError::Transport(_))
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Transport(msg) | McpError::Protocol(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for McpError {}

impl From<McpError> for String {
    fn from(error: McpError) -> Self {
        error.to_string()
    }
}

pub struct McpLoadResult {
    pub tools: Vec<Arc<dyn Tool>>,
    pub statuses: Vec<(String, McpConnectionStatus)>,
    /// The reconnect-capable server handles, one per connected server. The
    /// background refresh loop uses these to reconnect/re-discover tools.
    pub servers: Vec<Arc<McpServer>>,
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

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut transport = self.transport.lock().await;
        write_message(&mut transport.stdin, &payload)
            .await
            .map_err(|msg| McpError::Transport(format!("MCP {method} write failed: {msg}")))?;

        // Bound how long a single request can hang the agent: a server that
        // keeps the pipe open without answering is released rather than
        // pinning the transport lock forever.
        let response = timeout(MCP_REQUEST_TIMEOUT, async {
            loop {
                let response = read_message(&mut transport.stdout).await.map_err(|msg| {
                    McpError::Transport(format!("MCP {method} read failed: {msg}"))
                })?;
                if response.get("id").and_then(Value::as_u64) != Some(id) {
                    // An unrelated notification/async reply: skip, keep reading
                    // for *our* id.
                    continue;
                }
                // Our reply — break out of the async block with it.
                break Ok(response);
            }
        })
        .await
        .map_err(|_| {
            McpError::Transport(format!(
                "MCP {method} timed out after {}s",
                MCP_REQUEST_TIMEOUT.as_secs()
            ))
        })??;

        // At this point the response is well-formed and carries our id: a
        // JSON-RPC `error` object is a *protocol* result, not a transport
        // failure — the caller must NOT reconnect-and-retry it.
        if let Some(error) = response.get("error") {
            return Err(McpError::Protocol(format!("MCP {method} error: {error}")));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| McpError::Protocol(format!("MCP {method} response has no result")))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut transport = self.transport.lock().await;
        write_message(&mut transport.stdin, &payload)
            .await
            .map_err(|msg| McpError::Transport(format!("MCP {method} notify failed: {msg}")))
    }
}

struct McpTool {
    public_name: String,
    original_name: String,
    description: String,
    parameters: Value,
    /// The reconnect-capable server handle. When a tool call fails with a
    /// connection error, [`call`](Tool::call) resets the connection and retries
    /// once — transparent crash recovery without waiting for the next refresh.
    server: Arc<McpServer>,
}

/// A reconnect-capable MCP server connection. Wraps `McpClient` with the
/// original config so a crashed server (stdout closed mid-session) can be
/// transparently restarted. Used by `McpTool::call` to retry on connection
/// failure.
pub struct McpServer {
    config: McpServerConfig,
    server_name: String,
    read_only: bool,
    client: tokio::sync::Mutex<Option<Arc<McpClient>>>,
}

impl McpServer {
    pub fn new(config: McpServerConfig, server_name: String, read_only: bool) -> Self {
        Self {
            config,
            server_name,
            read_only,
            client: tokio::sync::Mutex::new(None),
        }
    }

    /// Connect (or reconnect) and return the live client. If a client is
    /// already held, it is reused; otherwise a fresh connection is established.
    async fn ensure_connected(&self) -> Result<Arc<McpClient>, String> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let client = McpClient::connect(&self.config).await?;
        *guard = Some(client.clone());
        Ok(client)
    }

    /// Drop the current connection so the next `ensure_connected` reconnects.
    /// Called when a request fails with a connection error.
    pub async fn reset(&self) {
        *self.client.lock().await = None;
    }

    /// The server's display name (for logging/diagnostics).
    pub fn name(&self) -> &str {
        &self.server_name
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }
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

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let arguments: Value = serde_json::from_str(arguments)
            .map_err(|error| format!("invalid MCP tool arguments: {}", error))?;
        let payload = json!({
            "name": self.original_name,
            "arguments": arguments,
        });
        // Try with the current (possibly cached) connection. Only a *transport*
        // failure (the pipe broke — the server crashed) warrants a reconnect
        // and a single retry. A *protocol* error (a JSON-RPC `error` object) is
        // a deterministic server-side result: re-sending an already-applied
        // non-idempotent tool call would repeat its side effect, so it is
        // returned to the caller verbatim and never retried.
        let client = self.server.ensure_connected().await?;
        match client.request("tools/call", payload.clone()).await {
            Ok(result) => Ok(render_tool_result(&result)),
            Err(error) if error.is_transport() => {
                tracing::warn!(
                    server = %self.server.name(),
                    tool = %self.original_name,
                    %error,
                    "MCP transport error, reconnecting and retrying once"
                );
                self.server.reset().await;
                let client = self.server.ensure_connected().await?;
                let result = client.request("tools/call", payload).await?;
                Ok(render_tool_result(&result))
            }
            Err(error) => {
                // Protocol error: surface it to the caller; do not reconnect.
                Err(error.to_string())
            }
        }
    }
}

pub async fn load_mcp_tools(configs: &HashMap<String, McpServerConfig>) -> McpLoadResult {
    let mut tools = Vec::new();
    let mut statuses = Vec::new();
    let mut servers = Vec::new();
    let mut names = configs.keys().cloned().collect::<Vec<_>>();
    names.sort();

    for name in names {
        let config = &configs[&name];
        if !config.enabled {
            statuses.push((name, McpConnectionStatus::Disabled));
            continue;
        }

        let loaded = timeout(MCP_CONNECT_TIMEOUT, async {
            let server = Arc::new(McpServer::new(
                config.clone(),
                name.clone(),
                config.read_only,
            ));
            let server_tools = build_tools_from_server(&server).await?;
            Ok::<_, String>((server, server_tools))
        })
        .await;

        match loaded {
            Ok(Ok((server, server_tools))) => {
                let count = server_tools.len();
                tools.extend(server_tools);
                statuses.push((name, McpConnectionStatus::Connected { tools: count }));
                servers.push(server);
            }
            Ok(Err(error)) => statuses.push((name, McpConnectionStatus::Failed(error))),
            Err(_) => statuses.push((
                name,
                McpConnectionStatus::Failed("connection timed out".to_string()),
            )),
        }
    }

    McpLoadResult {
        tools,
        statuses,
        servers,
    }
}

/// Reconnect to each server and re-discover its tools. Returns the refreshed
/// tool list and statuses (same shape as [`load_mcp_tools`], but reusing
/// existing server handles). Called by the background refresh loop.
pub async fn refresh_mcp_tools(
    servers: &[Arc<McpServer>],
) -> (Vec<Arc<dyn Tool>>, Vec<(String, McpConnectionStatus)>) {
    let mut tools = Vec::new();
    let mut statuses = Vec::new();

    for server in servers {
        // Reset the connection so ensure_connected opens a fresh one.
        server.reset().await;
        match timeout(MCP_CONNECT_TIMEOUT, async {
            build_tools_from_server(server).await
        })
        .await
        {
            Ok(Ok(server_tools)) => {
                let count = server_tools.len();
                tools.extend(server_tools);
                statuses.push((
                    server.name().to_string(),
                    McpConnectionStatus::Connected { tools: count },
                ));
            }
            Ok(Err(error)) => {
                statuses.push((
                    server.name().to_string(),
                    McpConnectionStatus::Failed(error),
                ));
            }
            Err(_) => statuses.push((
                server.name().to_string(),
                McpConnectionStatus::Failed("connection timed out".to_string()),
            )),
        }
    }

    (tools, statuses)
}

/// Connect a single server from its config and discover its tools. Returns the
/// reconnect-capable handle alongside its tool adapters, or a failure string.
/// Used to bring a server online on demand (e.g. the `/mcp` modal re-enabling a
/// previously disabled server, which never went through [`load_mcp_tools`]).
pub async fn connect_server(
    name: &str,
    config: &McpServerConfig,
) -> Result<(Arc<McpServer>, Vec<Arc<dyn Tool>>), String> {
    timeout(MCP_CONNECT_TIMEOUT, async {
        let server = Arc::new(McpServer::new(
            config.clone(),
            name.to_string(),
            config.read_only,
        ));
        let tools = build_tools_from_server(&server).await?;
        Ok::<_, String>((server, tools))
    })
    .await
    .map_err(|_| "connection timed out".to_string())?
}

/// Reset and re-establish one server's connection, re-discovering its tools.
/// The single-server analogue of [`refresh_mcp_tools`]; returns the refreshed
/// tools and the connection status. Used by the `/mcp` modal's reconnect action.
pub async fn reconnect_server(
    server: &Arc<McpServer>,
) -> (Vec<Arc<dyn Tool>>, McpConnectionStatus) {
    server.reset().await;
    match timeout(MCP_CONNECT_TIMEOUT, build_tools_from_server(server)).await {
        Ok(Ok(tools)) => {
            let count = tools.len();
            (tools, McpConnectionStatus::Connected { tools: count })
        }
        Ok(Err(error)) => (Vec::new(), McpConnectionStatus::Failed(error)),
        Err(_) => (
            Vec::new(),
            McpConnectionStatus::Failed("connection timed out".to_string()),
        ),
    }
}

/// Build [`McpTool`] adapters from a connected server's `tools/list` response.
/// Each tool holds the [`McpServer`] handle so it can auto-reconnect on
/// failure.
async fn build_tools_from_server(server: &Arc<McpServer>) -> Result<Vec<Arc<dyn Tool>>, String> {
    let client = server.ensure_connected().await?;
    let result = client.request("tools/list", json!({})).await?;
    let definitions = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "MCP tools/list response has no tools array".to_string())?;

    // `sanitize_name` is a lossy, many-to-one mapping (every non-`[A-Za-z0-9_]`
    // char folds to `_`), so distinct server-side tool names can collapse to the
    // same public name (e.g. `read-file` and `read.file` both become
    // `read_file`). Left unchecked, the second adapter would be silently dropped
    // by the first-wins `ToolSet`, making that tool unreachable. We guarantee
    // uniqueness here, where the whole server's tool list is in scope, by
    // appending the lowest free numeric suffix on collision. `original_name` is
    // untouched, so `tools/call` still targets the correct server-side tool.
    let mut taken = HashSet::new();
    definitions
        .iter()
        .map(|definition| {
            let original_name = definition
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "MCP tool has no name".to_string())?
                .to_string();
            let public_name = unique_name(
                format!(
                    "mcp__{}__{}",
                    sanitize_name(server.name()),
                    sanitize_name(&original_name)
                ),
                &mut taken,
            );
            let description = definition
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("MCP tool")
                .to_string();
            let parameters = definition
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object"}));
            Ok(Arc::new(McpTool {
                public_name,
                original_name,
                description,
                parameters,
                server: Arc::clone(server),
            }) as Arc<dyn Tool>)
        })
        .collect()
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

/// Reserve a collision-free variant of `name` within `taken`, recording the
/// result. The first claimant keeps the bare name; each later duplicate gets the
/// lowest free `_2`, `_3`, … suffix. Deterministic and order-stable, so a server
/// returning the same `tools/list` always yields the same public names.
fn unique_name(name: String, taken: &mut HashSet<String>) -> String {
    if taken.insert(name.clone()) {
        return name;
    }
    let mut suffix = 2u32;
    loop {
        let candidate = format!("{name}_{suffix}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
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
    fn colliding_public_names_are_disambiguated_deterministically() {
        // `read-file` and `read.file` both sanitize to the same public name;
        // the second (and third) must get a distinct, stable suffix.
        let mut taken = HashSet::new();
        let first = unique_name("mcp__s__read_file".to_string(), &mut taken);
        let second = unique_name("mcp__s__read_file".to_string(), &mut taken);
        let third = unique_name("mcp__s__read_file".to_string(), &mut taken);
        assert_eq!(first, "mcp__s__read_file");
        assert_eq!(second, "mcp__s__read_file_2");
        assert_eq!(third, "mcp__s__read_file_3");
        // A name that only differs after sanitization keeps its own identity.
        let other = unique_name("mcp__s__write_file".to_string(), &mut taken);
        assert_eq!(other, "mcp__s__write_file");
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
