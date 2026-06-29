# MCP servers

neenee connects to local stdio [Model Context Protocol][mcp] servers and exposes
their tools alongside the built-in ones, using the same execution path. A shared
**MCP runtime** owns the live connections: it connects every configured server at
startup, keeps the agent's tool list in sync, and recovers crashed servers. This
page covers the runtime and its recovery model, the tool wrapper, the `/mcp`
manager, and how MCP tools interact with [envoy admission](envoys.md) and
the permission broker. For the per-tool parameter surface, see
[Built-in tools](../../reference/tools/index.md).

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
3. **Failure-isolated, self-healing.** A server that times out or crashes
   cannot prevent neenee from starting, and it is reconnected automatically (see
   [Failure isolation and recovery](#failure-isolation-and-recovery)).

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
| `enabled` | `true` | When `false`, the server is recorded as disabled and not spawned at startup (still enableable live from `/mcp`) |
| `read_only` | `false` | Sets the tool's access tier; gates [envoy admission](#access-tier-and-envoy-admission) and the permission broker |

`command` is argv-style, not a shell string — users pre-split it
(`["npx", "-y", "..."]`). The map key is the server name, which becomes the
first segment of the public tool name.

## The runtime

The MCP runtime is the single source of truth for which servers are connected,
their per-server tools, and their status. It owns the agent's shared **tool
holder** — the live list the model sees — and rewrites it (the union of every
connected server's tools) on every change. So enabling, disabling, reconnecting,
or a periodic refresh all flow through one place, and the agent always sees
exactly the servers that are up.

At startup the runtime connects every enabled server, sorted by name for
deterministic order:

```text
sort server names
for each name:
  if !enabled            → record disabled, no spawn
  else timeout(8s, …):
    spawn child
    initialize (protocolVersion 2024-11-05) + notifications/initialized
    tools/list
    wrap each definition in an MCP tool (sharing one client handle)
  →  connected{tools: count}  |  failed(error)  |  failed("connection timed out")
```

The 8-second connect timeout bounds the whole spawn + initialize + `tools/list`
sequence. There is **no per-call timeout** on `tools/call` — a hung server
blocks that one tool call indefinitely.

Connecting never returns a single `Result`: neenee always starts, with whatever
tools came up. MCP tools sit in the toolset after the built-ins and before the
envoy tool, so they are visible to [envoys](envoys.md) when the
server is `read_only`.

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

## Failure isolation and recovery

The per-server outcome never propagates: each server resolves to one status —
**connected** (tools registered), **disabled**, or **failed** (a spawn that
came up but whose `initialize`/`tools/list` errored, or the 8-second connect
timeout elapsing). A failed server contributes zero tools and one status row;
the others are unaffected.

A down server does not stay down. Four recovery paths overlap, cheapest first:

1. **Per-call reconnect.** When a `tools/call` fails on a connection error (the
   child crashed since the last call), the wrapper resets the connection,
   reconnects, and retries the call once — transparent to the model.
2. **Periodic refresh.** Every 10 minutes a background catalog loop reconnects
   every enabled server and re-runs `tools/list`, so a recovered server returns
   and newly-exposed tools appear without a restart.
3. **On-demand reconnect.** The `/mcp` manager's `r` action reconnects the
   selected server immediately, for when you don't want to wait for the refresh.
4. **Restart.** Always available; rereads `config.toml` from scratch.

Because the runtime rewrites the agent's tool holder on every one of these, the
model's visible toolset tracks the live connection state automatically.

## The `/mcp` manager

`/mcp` opens a modal listing every configured server with its status glyph,
name, tool count or failure reason, and an on/off badge:

```text
  ● filesystem   4 tools          [on]
  ○ git          disabled         [off]
  ✕ database     failed: timeout  [on]
```

Two per-row actions drive the runtime live, without rewriting `config.toml`:

- **`Space`** connects or disconnects the selected server for the session.
  Disabling drops its tools and kills the child; enabling reconnects it from
  `[mcp.<name>]` and re-discovers its tools. Session-scoped — a restart restores
  the configured `enabled` state.
- **`r`** reconnects the selected server (recovery path 3 above).

`/mcp` shows each server's status and tool count as a glanceable list, and is
the surface for acting on it.

## Access tier and envoy admission

An MCP tool's `read_only` flag sets its `ToolAccess` tier, which an envoy
profile admits by capability axis (ADR-0011). Every built-in envoy profile
carries a `Read` ceiling, so only `read_only = true` MCP servers are usable
inside an envoy; a server that needs to run inside one must declare
`read_only = true`. Outside envoys the main agent is unrestricted, so an
MCP tool's tier only gates its *envoy* admission and the permission broker
(below), never the main agent.

- `read_only = true` server → `Read` → admitted by every built-in profile.
- `read_only = false` server (the default) → `Write` → admitted only by a
  profile that grants writes (then scoped by `WriteScope`); no built-in
  profile does today.

## Permission broker

MCP write tools pass through the broker like any `Write` tool. Every call is
scoped `"*"`, so a cached `Always` rule is keyed by the full
`mcp__<server>__<tool>` name — a user can blanket-approve one specific MCP
tool but not "every tool from server X". `/permissions clear` revokes the
cache.

The permission prompt title is the synthetic `mcp__…` name. Readable, but not
pretty; a human-friendly override is a candidate follow-up.

## Lifecycle

The child process is constructed once per connection, wrapped in a shared
handle, and cloned into every tool wrapper from that server. While connected,
one server means one process; tool calls multiplex over the same stdin/stdout.
The child is killed when its handle is dropped — on shutdown, on a reconnect
(the old handle is replaced), or when the server is disabled from `/mcp`. There
is no graceful `shutdown` JSON-RPC exchange — the child is sent `SIGKILL`.

## See also

- [Built-in tools](../../reference/tools/index.md) — `mcp__<server>__<tool>`
  parameter surface and the MCP tools subsection
- [Envoys](envoys.md) — why `read_only` MCP servers are visible
  to envoys and write servers are not
- [Harness architecture](harness.md) — the 8-second MCP init bound and the
  tool permission broker
