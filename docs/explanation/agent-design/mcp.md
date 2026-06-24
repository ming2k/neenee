# MCP servers

neenee discovers local stdio [Model Context Protocol][mcp] servers at startup
and exposes their tools alongside the built-in ones, using the same execution
path. This page covers discovery, the tool wrapper, failure isolation, and how
MCP tools interact with [Plan mode](plan-mode.md) and the permission broker.
For the per-tool parameter surface, see [Built-in tools](../../reference/tools/index.md).

[mcp]: https://modelcontextprotocol.io/

## Why MCP support

Built-in tools cover the common cases — file access, search, bash, web. MCP
lets a project add capabilities without forking neenee: a database query tool,
a private API client, a custom linter. The integration is deliberately narrow:

1. **Same execution path.** An MCP tool shares the `Tool` trait, the
   permission broker, the [tool-round](turns-and-rounds.md) loop, and the TUI step
   renderer with every built-in. The agent does not treat MCP tools specially.
2. **Local stdio only.** neenee speaks JSON-RPC over a spawned child's
   stdin/stdout. No HTTP, no remote servers — the server runs under the user's
   account and filesystem.
3. **Failure-isolated.** A server that times out or crashes cannot prevent
   neenee from starting.

## Configuration

Each server is one `[mcp.<name>]` table in `config.toml`:

```toml
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."]
enabled = true
read_only = false
```

| Field | Default | Meaning |
|-------|---------|---------|
| `command` | — | argv; first element is the program |
| `environment` | empty | Env vars applied at spawn |
| `enabled` | `true` | When `false`, the server is recorded as disabled and never spawned |
| `read_only` | `false` | Sets the tool's access tier; gates [Plan mode](#plan-mode) and the permission broker |

`command` is argv-style, not a shell string — users pre-split it
(`["npx", "-y", "..."]`). The map key is the server name, which becomes the
first segment of the public tool name.

## Discovery and registration

MCP tools load once at startup. Server names are sorted for deterministic
order. Per server:

```text
sort server names
for each name:
  if !enabled            → record disabled, skip spawn
  else timeout(8s, …):
    spawn child
    initialize (protocolVersion 2024-11-05) + notifications/initialized
    tools/list
    wrap each definition in an MCP tool (sharing one client handle)
  match:
    Ok(Ok(tools))  → extend registry, record connected{tools: count}
    Ok(Err(error)) → record failed(error)
    Err(timeout)   → record failed("connection timed out")
```

The 8-second connect timeout bounds the whole spawn + initialize + `tools/list`
sequence. There is **no per-call timeout** on `tools/call` — a hung server
blocks that one tool call indefinitely.

The load result is a list of tools plus a status per server, not a single
`Result`: neenee always starts, with whatever tools loaded. MCP tools are
spliced into the agent's toolset after the built-ins and before the sub-agent
tool, so they are visible to [sub-agents](subagents.md) when the server
is `read_only`.

## The tool wrapper

Each advertised tool becomes a private wrapper implementing `Tool`. Three
transformations happen at wrap time:

1. **Public name.** `mcp__{sanitize(server)}__{sanitize(original)}`. The
   sanitizer keeps ASCII alphanumerics and `_`, replacing everything else
   (including `-`, `.`, `/`) with `_`. This is required because the name
   becomes a provider function name, and providers reject names with slashes
   or spaces. Collisions are possible (`read-file` and `read.file` both become
   `read_file`) and not detected at load time.
2. **Schema.** The server's `inputSchema` is used verbatim, falling back to
   `{"type":"object"}` when absent so the OpenAI function definition stays
   valid.
3. **Access.** The server-level `read_only` flag becomes the tool's access
   tier: `Read` when set, `Write` otherwise. Every tool from one server shares
   the same access.

At call time, the wrapper dispatches `tools/call` over JSON-RPC using the
**original** server-side tool name, not the sanitized public one. The wrapper
preserves the original name to call through and exposes the sanitized name to
the model.

## JSON-RPC transport

Framing is **newline-delimited JSON** — one JSON-RPC message per line, blank
lines skipped. This is the line-delimited variant the MCP spec allows; neenee
does not implement the `Content-Length` framing.

All requests are serialized through one mutex over the single stdin/stdout
pair — no pipelining. The response loop silently skips messages whose `id`
does not match, which handles stray server-initiated notifications. Tool
results join every `{"type":"text","text": …}` block in the `content` array
with newlines and drop non-text blocks, falling back to raw JSON serialization
when no text content is present.

## Failure isolation

The per-server outcome never propagates. Three outcomes are recorded and the
loop continues to the next server:

- healthy — tools registered.
- spawn succeeded but `initialize` or `tools/list` returned a transport or
  JSON-RPC error; recorded as failed.
- the 8-second connect timeout elapsed; recorded as failed
  ("connection timed out").

A failed server contributes zero tools and one status row. There is **no
reconnection**: if a child crashes mid-session, the next `tools/call` returns
an error and the server stays down until neenee restarts.

`/mcp` renders the cached statuses as plain text:

```text
MCP servers:
- filesystem: connected (4 tools)
- git: failed: connection timed out
```

The session modal (`Ctrl+I`) shows the same data plus per-server tool names.

## Plan mode

An MCP tool inherits the default plan-mode check: it is admitted only when its
access tier is `Read`. The consequence is exactly the inverse of the
`.neenee/plans/` write exemption that built-in file tools enjoy:

- `read_only = true` server → `Read` → **allowed** in Plan mode.
- `read_only = false` server (the default) → `Write` → **blocked** in Plan
  mode, with no per-invocation exemption.

A project that wants an MCP tool usable during planning must opt in by setting
`read_only = true` in config.

## Permission broker

MCP write tools pass through the broker like any `Write` tool. Every call is
scoped `"*"`, so a cached `Always` rule is keyed by the full
`mcp__<server>__<tool>` name — a user can blanket-approve one specific MCP
tool but not "every tool from server X". `/permissions clear` revokes the
cache.

The permission prompt title is the synthetic `mcp__…` name. Readable, but not
pretty; a human-friendly override is a candidate follow-up.

## Lifecycle

The child process is constructed once per server, wrapped in a shared handle,
and cloned into every tool wrapper from that server. One server means one
process for the whole session; tool calls multiplex over the same
stdin/stdout. The child is killed when that shared handle is dropped on
shutdown. There is no graceful `shutdown` JSON-RPC exchange — the child is
sent `SIGKILL`.

## See also

- [Built-in tools](../../reference/tools/index.md) — `mcp__<server>__<tool>`
  parameter surface and the MCP tools subsection
- [Plan mode](plan-mode.md) — the access-tier gate and the `.neenee/plans/`
  write exemption
- [Sub-agents](subagents.md) — why `read_only` MCP servers are visible
  to sub-agents and write servers are not
- [Harness architecture](harness.md) — the 8-second MCP init bound and the
  tool permission broker
