//! End-to-end check of the MCP stdio client against a real subprocess.
//!
//! Spawns the dependency-free `mock_mcp_server.py` fixture through the actual
//! `load_mcp_tools` path, then exercises tool discovery, a tool call, and the
//! disabled-server short-circuit. Skips (rather than fails) when `python3` is
//! unavailable so CI without Python stays green.

use std::collections::HashMap;
use std::path::PathBuf;

use neenee_core::mcp::{McpConnectionStatus, McpServerConfig};
use neenee_tools::mcp::load_mcp_tools;

fn python3() -> Option<String> {
    let probe = std::process::Command::new("python3")
        .arg("--version")
        .output();
    matches!(probe, Ok(out) if out.status.success()).then(|| "python3".to_string())
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_mcp_server.py")
}

fn server_config(python: &str) -> McpServerConfig {
    McpServerConfig {
        command: vec![python.to_string(), fixture().to_string_lossy().into_owned()],
        environment: HashMap::new(),
        enabled: true,
        read_only: false,
    }
}

#[tokio::test]
async fn discovers_and_calls_tools_over_stdio() {
    let Some(python) = python3() else {
        eprintln!("skipping: python3 not available");
        return;
    };

    let mut configs = HashMap::new();
    configs.insert("mock".to_string(), server_config(&python));

    let result = load_mcp_tools(&configs).await;

    // Two tools advertised, each namespaced under the server.
    assert_eq!(
        result.statuses,
        vec![("mock".to_string(), McpConnectionStatus::Connected { tools: 2 })],
    );
    let mut names: Vec<_> = result.tools.iter().map(|t| t.name().to_string()).collect();
    names.sort();
    assert_eq!(names, vec!["mcp__mock__add", "mcp__mock__echo"]);

    // A real tool call round-trips through the subprocess.
    let add = result
        .tools
        .iter()
        .find(|t| t.name() == "mcp__mock__add")
        .expect("add tool present");
    let sum = add.call(r#"{"a": 2, "b": 40}"#).await.expect("call ok");
    assert_eq!(sum, "42");
}

#[tokio::test]
async fn disabled_server_reports_disabled_and_loads_no_tools() {
    let mut config = server_config("python3");
    config.enabled = false;
    let mut configs = HashMap::new();
    configs.insert("mock".to_string(), config);

    let result = load_mcp_tools(&configs).await;

    assert!(result.tools.is_empty());
    assert_eq!(
        result.statuses,
        vec![("mock".to_string(), McpConnectionStatus::Disabled)],
    );
}
