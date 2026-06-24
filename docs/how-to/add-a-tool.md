# How to add a built-in tool

This guide walks through implementing a new tool that the agent can call. It
assumes familiarity with the `Tool` trait. For the existing tool catalog,
see [Built-in tools](../reference/tools/index.md). For the protocol the model uses
to call tools, see [Tool rounds](../explanation/agent-design/turns-and-rounds.md).

Most built-in tools live in the `neenee-tools` crate. Pick the module that
matches the tool's domain: filesystem and web tools go in
`crates/neenee-tools/src/lib.rs`, project scaffolding tools go in
`crates/neenee-tools/src/project.rs`, MCP integration lives in
`crates/neenee-tools/src/mcp.rs`. `use_skill` and `task` are the exceptions —
they live in `crates/neenee-agent/src/` because they need orchestration state.

## Implement the `Tool` trait

Define a struct and implement `Tool`
(`crates/neenee-core/src/capability.rs`). The four required members are
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

## Return structured output (`ToolOutput`)

Implement `call()` for the model-facing string result, then override
`call_structured()` (`crates/neenee-core/src/capability.rs`) to return a typed
[`ToolOutput`](../adr/0001-tool-rendering-redesign.md) so the UI renders from
data instead of a sniffed string. The default `call_structured()` just wraps
`call()`'s string as `ToolOutput::Text`, so this is optional but recommended
for any tool whose result has structure (a shell exit code, a file listing, a
diff, …). `call()` should delegate back through `to_text()` so both paths stay
consistent:

```rust
async fn call(&self, arguments: &str) -> Result<String, String> {
    self.call_structured(arguments).await.map(|o| o.to_text())
}

async fn call_structured(&self, arguments: &str) -> Result<crate::ToolOutput, String> {
    // …do the work…
    Ok(crate::ToolOutput::Code {
        lang: Some("rs".into()),
        text,
        start_line: 0,
        prefix: None,
        suffix: None,
    })
}
```

The variants (`Text`, `Error`, `Shell`, `Code`, `Listing`, `Matches`) live in
`crates/neenee-core/src/tool_output.rs`. `bash` is the reference example — it
also overrides `call_structured_with_events` to stream stdout live via
`ToolStream`.

## Choose a `ToolAccess`

Override `access()` (`crates/neenee-core/src/capability.rs`) only when the
tool is read-only. The default is `ToolAccess::Write`, which is the safe
choice for any tool with side effects.

```rust
fn access(&self) -> ToolAccess {
    ToolAccess::Read
}
```

`Read` tools bypass the permission broker and run in Plan mode. `Write`
tools prompt the user once per `(tool, scope)` pair unless an `Always` rule
is cached. See [Built-in tools](../reference/tools/access.md) for the
full gating matrix.

## Override `permission_scope` for write tools

A `Write` tool should override `permission_scope`
(`crates/neenee-core/src/capability.rs`) so cached `Always` rules match the
smallest stable resource identifier. The default `"*"` causes any approval
to authorize all future calls to that tool, which is rarely what users
want.

```rust
fn permission_scope(&self, arguments: &str) -> String {
    json_string(arguments, "path")
}
```

`json_string` (`crates/neenee-tools/src/lib.rs`) extracts a JSON field
from the arguments string and falls back to `"*"`. Existing scopes: file
tools use the `path` argument, `bash` uses the full `command` text,
`create_project` uses `{path}/{name}`. Pick a scope that distinguishes
meaningfully different invocations but is stable across retries of the same
invocation.

## Override `permission_label` / `permission_description` when needed

`Tool::description()` is sent to the model and is often written as
instruction prose ("Call this only when…", "Do not infer…"). That text is
fine for the model but confusing when the user reads it in a permission
prompt. Two trait methods control what the prompt shows instead
(`crates/neenee-core/src/capability.rs`):

- `permission_label()` (default: `name()`) — the header title.
- `permission_description()` (default: `description()`) — the body shown
  under "Details".

Override either only when the default would puzzle a user. Keep
`permission_description()` to one or two plain sentences describing *what
the call does*, not *when the model should call it*.

```rust
fn permission_label(&self) -> String {
    "Create pursuit".to_string()
}

fn permission_description(&self) -> String {
    "Start a new active pursuit for this thread, replacing any completed pursuit.".to_string()
}
```

`start_pursuit` and `complete_pursuit` (`crates/neenee-core/src/pursuits/tools.rs`)
are the reference implementation. Both overrides are UI-only: they never
reach the model and are not part of the function schema.

## Optional: stream sub-task events

If the tool spawns long-running work that should surface in the TUI,
override `call_with_events` (`crates/neenee-core/src/capability.rs`) instead
of `call`. The default implementation delegates to `call`, so overriding
`call` alone is enough for synchronous tools.

`TaskTool` (`crates/neenee-agent/src/task_tool.rs`) is currently the only
tool that overrides `call_with_events`. It forwards `SubTaskEvent`s from
the sub-agent so the parent harness can render live progress. Read its
implementation before adopting the same pattern; the event surface is
narrow.

## Register the tool

Add the tool to the literal registry in `crates/neenee-cli/src/main.rs` (the
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
sub-agent, which snapshots the assembled toolset at construction and then
admits tools via the bound `EXPLORE` profile
(`crates/neenee-core/src/subagent.rs`). Admission is by capability axis, not
position: a tool is sub-agent-callable when `ToolPolicy::admits` accepts it —
`access() == Read`, not `requires_user()`, and not `spawns_subagent()`. Tools
added after that line (the dispatch tool itself, the history tool) are not in
the snapshot at all. Place new read-only, non-interactive tools before the MCP
extension to make them sub-agent-callable; write tools and `requires_user()`
tools are excluded by the profile regardless of where they sit. See
[Sub-agents → Tool admission](../explanation/agent-design/subagents.md#tool-admission)
and [ADR-0011](../adr/0011-subagent-profiles.md).

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
3. Confirm the tool step renders with the right name, arguments, and
   result.
4. Switch to `GeminiProvider` or `LlamaServerProvider` and repeat. The
   model should emit the universal fallback JSON and the tool should still
   execute through `parse_tool_call`.

If the tool is `Write`, also confirm the permission modal appears on first
use and that an `Always` decision is cached against the scope returned by
`permission_scope`.

## Update documentation

Update these surfaces in the same change:

- Add a row to the table in [Built-in tools](../reference/tools/index.md).
- If the tool introduces a new permission scope shape, document it under
  the tool's parameter table.
- If the tool changes how the harness behaves on a turn, update
  [Harness architecture](../explanation/agent-design/harness.md).

## See also

- [Built-in tools](../reference/tools/index.md) — existing tool catalog
- [Tool rounds](../explanation/agent-design/turns-and-rounds.md) — schema injection and
  fallback mechanics
- [Provider capabilities](../explanation/provider-capabilities.md) — why
  tool support varies across providers
