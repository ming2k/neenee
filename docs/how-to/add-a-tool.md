# How to add a built-in tool

This guide walks through implementing a new tool that the agent can call. It
assumes familiarity with the `Tool` trait. For the existing tool catalog,
see [Built-in tools](../reference/tools.md). For the protocol the model uses
to call tools, see [Tool protocol](../explanation/tool-protocol.md).

All production tools live in the `neenee-core` crate. Pick the module that
matches the tool's domain: filesystem tools go in
`crates/neenee-core/src/tools.rs`, project scaffolding tools go in
`crates/neenee-core/src/project.rs`, MCP integration lives in
`crates/neenee-core/src/mcp.rs`.

## Implement the `Tool` trait

Define a struct and implement `Tool`
(`crates/neenee-core/src/lib.rs`). The four required members are
`name`, `description`, `parameters`, and `call`.

```rust
pub struct CountLinesTool;

#[async_trait]
impl Tool for CountLinesTool {
    fn name(&self) -> &str {
        "count_lines"
    }

    fn description(&self) -> &str {
        "Count the number of lines in a file."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let path = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(str::to_string))
            .ok_or("missing \"path\"")?;
        let content = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())?;
        Ok(content.lines().count().to_string())
    }
}
```

`parameters()` returns a JSON Schema. It is forwarded verbatim to the model
through `Tool::to_openai_function()`; no tool overrides that
default. Keep the schema strict: set `additionalProperties: false` and list
every required field so the model cannot invent extra keys.

## Choose a `ToolAccess`

Override `access()` (`crates/neenee-core/src/lib.rs`) only when the
tool is read-only. The default is `ToolAccess::Write`, which is the safe
choice for any tool with side effects.

```rust
fn access(&self) -> ToolAccess {
    ToolAccess::Read
}
```

`Read` tools bypass the permission broker and run in Plan mode. `Write`
tools prompt the user once per `(tool, scope)` pair unless an `Always` rule
is cached. See [Built-in tools](../reference/tools.md#tool-access) for the
full gating matrix.

## Override `permission_scope` for write tools

A `Write` tool should override `permission_scope`
(`crates/neenee-core/src/lib.rs`) so cached `Always` rules match the
smallest stable resource identifier. The default `"*"` causes any approval
to authorize all future calls to that tool, which is rarely what users
want.

```rust
fn permission_scope(&self, arguments: &str) -> String {
    json_string(arguments, "path")
}
```

`json_string` (`crates/neenee-core/src/tools.rs`) extracts a JSON field
from the arguments string and falls back to `"*"`. Existing scopes: file
tools use the `path` argument, `bash` uses the full `command` text,
`create_project` uses `{path}/{name}`. Pick a scope that distinguishes
meaningfully different invocations but is stable across retries of the same
invocation.

## Optional: stream sub-task events

If the tool spawns long-running work that should surface in the TUI,
override `call_with_events` (`crates/neenee-core/src/lib.rs`) instead
of `call`. The default implementation delegates to `call`, so overriding
`call` alone is enough for synchronous tools.

`TaskTool` (`crates/neenee-core/src/tools.rs`) is currently the only
tool that overrides `call_with_events`. It forwards `SubTaskEvent`s from
the sub-agent so the parent harness can render live progress. Read its
implementation before adopting the same pattern; the event surface is
narrow.

## Register the tool

Add the tool to the literal registry in `crates/neenee/src/main.rs` (the
`let mut tools: Vec<Arc<dyn neenee_core::Tool>> = vec![ … ]` block),
preserving the existing order (write tools first, then read tools, then
`use_skill`, then MCP extension).

```rust
let mut tools: Vec<Arc<dyn neenee_core::Tool>> = vec![
    Arc::new(BashTool),
    Arc::new(ReadFileTool),
    // ...
    Arc::new(CountLinesTool),  // new
];
```

Tools added before `tools.extend(mcp.tools)` are visible to the `task`
sub-agent, which inherits the parent toolset filtered to `Read`. Tools
added after that line are not. Place read-only tools before the MCP
extension to make them sub-agent-callable; place write tools or tools that
should never recurse anywhere in the list. `TaskTool` is pushed last, after
the MCP extension, so it snapshots the fully assembled toolset.

## Verify

Run the test suite before relying on the new tool:

```bash
cargo test -p neenee-core
cargo test -p neenee
```

Then exercise the tool manually:

1. Start the agent with a provider that supports native function calling
   (see [Providers](../reference/providers.md)).
2. Ask the model to perform a task that should trigger the new tool.
3. Confirm the tool-step card renders with the right name, arguments, and
   result.
4. Switch to `GeminiProvider` or `LlamaServerProvider` and repeat. The
   model should emit the universal fallback JSON and the tool should still
   execute through `parse_tool_call`.

If the tool is `Write`, also confirm the permission modal appears on first
use and that an `Always` decision is cached against the scope returned by
`permission_scope`.

## Update documentation

Update these surfaces in the same change:

- Add a row to the table in [Built-in tools](../reference/tools.md).
- If the tool introduces a new permission scope shape, document it under
  the tool's parameter table.
- If the tool changes how the harness behaves on a turn, update
  [Harness architecture](../explanation/harness.md).

## See also

- [Built-in tools](../reference/tools.md) — existing tool catalog
- [Tool protocol](../explanation/tool-protocol.md) — schema injection and
  fallback mechanics
- [Provider capabilities](../explanation/provider-capabilities.md) — why
  tool support varies across providers
