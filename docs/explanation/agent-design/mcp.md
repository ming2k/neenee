# MCP servers

neenee discovers local stdio [Model Context Protocol][mcp] servers at startup
and exposes their tools alongside the built-in ones, using the same execution
path. This page covers discovery, the tool wrapper, failure isolation, and how
MCP tools interact with [Plan mode](plan-mode.md) and the permission broker.
For the per-tool parameter surface, see [Built-in tools](../../reference/tools.md).

[mcp]: https://modelcontextprotocol.io/

## Why MCP support

Built-in tools cover the common cases — file access, search, bash, web. MCP
lets a project add capabilities without forking neenee: a database query tool,
a private API client, a custom linter. The integration is deliberately narrow:

1. **Same execution path.** An MCP tool shares the `Tool` trait, the
   permission broker, the [tool-round](tool-rounds.md) loop, and the TUI step
   renderer with every built-in. The agent does not treat MCP tools specially.
2. **Local stdio only.** neenee speaks JSON-RPC over a spawned child's
   stdin/stdout. No HTTP, no remote servers — the server runs under the user's
   account and filesystem.
3. **Failure-isolated.** A server that times out or crashes cannot prevent
   neenee from starting.

## The two files

The MCP implementation is split across two crates by the
[layering](harness.md) rules:

| File | Contents | Why |
|------|----------|-----|
| `crates/neenee-core/src/mcp.rs` | `McpServerConfig`, `McpConnectionStatus` | Both the store's `Config` and the loader need the type; `neenee-store` does not depend on `neenee-tools` |
| `crates/neenee-tools/src/mcp.rs` | `McpTransport`, `McpClient`, `McpTool`, `load_mcp_tools` | The client spawns a child process — an I/O concern that belongs in the tools crate |

## Configuration

Each server is one `[mcp.<name>]` table in `config.toml`, deserialized into
`McpServerConfig` (`crates/neenee-core/src/mcp.rs:11`):

```toml
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."]
enabled = true
read_only = false
```

| Field | Type | Default | Meaning |
|-------|------|---------|---------|
| `command` | `Vec<String>` | — | argv; first element is the program |
| `environment` | `HashMap<String,String>` | empty | Env vars applied at spawn |
| `enabled` | `bool` | `true` | When `false`, the server is recorded as `Disabled` and never spawned |
| `read_only` | `bool` | `false` | Maps to the tool's `ToolAccess`; gates [Plan mode](#plan-mode) and the permission broker |

`command` is argv-style, not a shell string. Users pre-split it
(`["npx", "-y", "..."]`), and `stderr` is dropped to `Stdio::null()`
(`crates/neenee-tools/src/mcp.rs:54`). The map key is the server name, which
becomes the first segment of the public tool name.

## Discovery and registration

`load_mcp_tools` (`crates/neenee-tools/src/mcp.rs:227`) runs once at startup.
Server names are sorted for deterministic order. Per server:

```text
sort server names
for each name:
  if !enabled            → record Disabled, skip spawn
  else timeout(8s, …):
    spawn child
    initialize (protocolVersion 2024-11-05) + notifications/initialized
    tools/list
    wrap each definition in McpTool (sharing one Arc<McpClient>)
  match:
    Ok(Ok(tools))  → extend registry, record Connected{tools: count}
    Ok(Err(error)) → record Failed(error)
    Err(timeout)   → record Failed("connection timed out")
```

The 8-second `MCP_CONNECT_TIMEOUT` (`mcp.rs:23`) bounds the whole
spawn + initialize + `tools/list` sequence. There is **no per-call timeout**
on `tools/call` — a hung server blocks that one tool call indefinitely.

The result is `McpLoadResult { tools, statuses }` (`mcp.rs:25`), not a
`Result`. The caller at `crates/neenee-cli/src/main.rs:363` never unwraps:
neenee always starts, with whatever tools loaded. MCP tools are spliced into
the agent's toolset at `main.rs:446`, after the built-ins and before
`TaskTool`, so they are visible to [sub-agents](subagents.md) when the server
is `read_only`.

## The `McpTool` wrapper

Each advertised tool becomes a private `McpTool` (`mcp.rs:183`) implementing
`Tool`. Three transformations happen at wrap time (`mcp.rs:152`):

1. **Public name.** `mcp__{sanitize(server)}__{sanitize(original)}`. The
   sanitizer (`mcp.rs:292`) keeps ASCII alphanumerics and `_`, replacing
   everything else (including `-`, `.`, `/`) with `_`. This is required because
   the name becomes a provider function name, and providers reject names with
   slashes or spaces. Collisions are possible (`read-file` and `read.file`
   both become `read_file`) and not detected at load time.
2. **Schema.** The server's `inputSchema` is used verbatim, falling back to
   `{"type":"object"}` when absent (`mcp.rs:162`) so the OpenAI function
   definition stays valid.
3. **Access.** The server-level `read_only` flag becomes the tool's
   `ToolAccess` (`mcp.rs:171`): `Read` when set, `Write` otherwise. Every tool
   from one server shares the same access.

At call time (`mcp.rs:213`), `McpTool::call` dispatches `tools/call` over
JSON-RPC using the **original** server-side tool name, not the sanitized
public one. The wrapper preserves the original name to call through and
exposes the sanitized name to the model.

## JSON-RPC transport

Framing is **newline-delimited JSON** — one JSON-RPC message per line, blank
lines skipped (`write_message`/`read_message` at `mcp.rs:263` and `mcp.rs:277`).
This is the line-delimited variant the MCP spec allows; neenee does not
implement the `Content-Length` framing.

All requests are serialized through `Mutex<McpTransport>` (`mcp.rs:37`) over
the single stdin/stdout pair — no pipelining. The response loop silently
skips messages whose `id` does not match, which handles stray
server-initiated notifications. `render_tool_result` (`mcp.rs:304`) joins
every `{"type":"text","text": …}` block in the `content` array with newlines
and drops non-text blocks, falling back to raw JSON serialization when no
text content is present.

## Failure isolation

The per-server `match` in `load_mcp_tools` never propagates. Three outcomes
are recorded and the loop continues to the next server:

- `Ok(Ok(tools))` — healthy; tools registered.
- `Ok(Err(error))` — spawn succeeded but `initialize` or `tools/list`
  returned a transport or JSON-RPC error; recorded as `Failed(error)`.
- `Err(_)` — the 8-second connect timeout elapsed; recorded as
  `Failed("connection timed out")`.

A failed server contributes zero tools and one status row. There is **no
reconnection**: if a child crashes mid-session, the next `tools/call` returns
`Err("MCP server closed stdout")` (`mcp.rs:282`) and the server stays down
until neenee restarts.

`/mcp` (`crates/neenee-cli/src/main.rs:838`) renders the cached statuses as
plain text via `McpConnectionStatus`'s `Display`:

```text
MCP servers:
- filesystem: connected (4 tools)
- git: failed: connection timed out
```

The session modal (`Ctrl+I`) shows the same data plus per-server tool names.

## Plan mode

`McpTool` does not override `allowed_in_plan_mode`, so it inherits the default
at `crates/neenee-core/src/capability.rs:90`: `access() == Read`. The Plan-mode
gate at `crates/neenee-agent/src/agent.rs:1490` consults that method and
returns a `[Plan mode] Tool '…' is blocked` string when it fails. The
consequence is exactly the inverse of `write_file`'s `.neenee/plans/`
exemption:

- `read_only = true` server → `Read` → **allowed** in Plan mode.
- `read_only = false` server (the default) → `Write` → **blocked** in Plan
  mode, with no per-invocation exemption.

A project that wants an MCP tool usable during planning must opt in by setting
`read_only = true` in config.

## Permission broker

MCP write tools pass through the broker like any `Write` tool. `McpTool` does
not override `permission_scope`, so every call is scoped `"*"`
(`crates/neenee-core/src/capability.rs:93`). A cached `Always` rule is keyed
by `(tool_name, scope)` — the full `mcp__<server>__<tool>` name — so a user
can blanket-approve one specific MCP tool but not "every tool from server X".
`/permissions clear` revokes the cache.

`permission_label` and `permission_description` also inherit their defaults,
so the prompt title is the synthetic `mcp__…` name. Readable, but not pretty;
a human-friendly override is a candidate follow-up.

## Lifecycle

The child process is constructed once per server in `McpClient::connect`
(`mcp.rs:42`), wrapped in `Arc`, and cloned into every `McpTool` from that
server. One server means one process for the whole session; tool calls
multiplex over the same stdin/stdout. The child is killed via tokio's
`kill_on_drop` (`mcp.rs:55`) when the `McpClient` `Arc` is dropped on
shutdown. There is no graceful `shutdown` JSON-RPC exchange — the child is
sent `SIGKILL`.

## See also

- [Built-in tools](../../reference/tools.md) — `mcp__<server>__<tool>` parameter
  surface and the MCP tools subsection
- [Plan mode](plan-mode.md) — the `allowed_in_plan_mode` mechanism and the
  `.neenee/plans/` write exemption
- [Sub-agents](subagents.md) — why `read_only` MCP servers are visible to
  sub-agents and write servers are not
- [Harness architecture](harness.md) — the 8-second MCP init bound and the
  tool permission broker
