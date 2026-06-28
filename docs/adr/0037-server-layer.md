# 0037. Session/server layer: `neenee-server` for multi-session daemon + multi-frontend

- **Status:** Accepted
- **Date:** 2026-07-14
- **Builds on:** ADR-0035 (application-layer split); ADR-0005 (strict layering)

## Context

`neenee-code` is currently a single process: one TUI driving one agent
background task over a pair of `mpsc` channels (`crates/neenee-code/src/main.rs`
lines 68-69 construct `req_tx`/`resp_tx`; `agent_loop::run` at
`crates/neenee-code/src/agent_loop.rs:113` is the single consumer of `req_rx`).
When the TUI process exits, the agent task dies with it — `main.rs:408`
`tokio::spawn(agent_loop::run(...))` is a detached task whose lifetime is the
process. This model cannot serve a browser frontend, which needs a
long-running daemon holding multiple concurrent sessions that several clients
can subscribe to.

The foundation for the fix is already in place:

- ADR-0005/0035 established a strictly-layered DAG where `neenee-agent` (the
  orchestration layer: `Agent`, `execute_turn`, `start_interactive_turn`) is
  fully decoupled from any frontend. `lib.rs` explicitly anticipates
  "menus/dialogs for a future GUI."
- `AgentRequest` / `AgentResponse` (`crates/neenee-core/src/events.rs`) are the
  only harness↔driver interface. They were `Debug`-only enums; a serialization
  prerequisite landed first (see Migration step 0).

The driver logic that would have to be shared — `agent_loop::Harness` +
`handlers/*` + `side` + `session_view` + `agent_setup` — is currently inlined
into `neenee-code` and is almost entirely TUI-free. An audit (`grep -n "tui::"
handlers/*.rs`) found exactly **one** TUI coupling point: the `/export` +
clipboard path in `handlers/slash.rs` (~30 lines, lines 970-992). Every other
handler takes only `&Agent`, `&SessionStore`, `&Config`, and channel handles.

The forces:

1. **A browser frontend needs a daemon.** The agent process must outlive any
   one client connection and hold state across reconnects.
2. **Multiple sessions must coexist.** A daemon holds N sessions; clients
   subscribe to the ones they care about. This is a new capability the current
   single-session `mpsc` shape cannot express.
3. **No duplication of driver logic.** The TUI and any web frontend must share
   the exact same turn dispatch, permission relay, compaction, and pursuit
   wiring — not parallel reimplementations.

## Decision

### 1. Add a `neenee-server` crate as the session/transport layer

A new crate sits between `neenee-agent` and the frontends:

```text
neenee-core        (no workspace deps)
       ^
       │
neenee-providers ─┐
neenee-tools      │  three peers; none depend on each other
neenee-store     ─┘
       ^
       │
neenee-agent       (core + store + providers; the Agent + orchestration)
       ^
       │
neenee-server      (NEW: session registry + transport; depends on agent)
       ^
       │
neenee-code  ─┐    application layer: each assembles its frontend and drives
(neenee-web) ─┘    a neenee-server process (embedded or remote).
```

The strict-DAG property from ADR-0005 is preserved: `neenee-server` adds one
node between `agent` and the applications and zero reverse edges.

### 2. Three core abstractions (already scaffolded)

| Type | File | Responsibility |
|------|------|----------------|
| `SharedState` | `crates/neenee-server/src/shared.rs` | Process-level singletons constructed once at bootstrap: the `ProxyProvider` + provider holder, skills registry, MCP statuses, config, embedding store, repeat store, project root. Mirrors `main.rs:96-278`. |
| `SessionRegistry` | `crates/neenee-server/src/registry.rs` | `session_id → Arc<SessionHandle>` map. `create_session` / `get` / `list` / `close_session`. |
| `SessionHandle` | `crates/neenee-server/src/registry.rs` | Per-session: the request sender + a **broadcast** response sender + creation instant. Clients send `AgentRequest`s in and subscribe to the `AgentResponse` stream. |

Each session owns its own `Agent` + `SessionStore` (provider/tools/pursuit/
thread_id/permissions cannot be shared across sessions — already proven by the
`/btw` side-session pattern, ADR-0017, `crates/neenee-code/src/side.rs`).
`SharedState` holds only true process globals.

### 3. Response channel: `mpsc` → `broadcast`

The single behavioral change versus the current loop: the harness→frontend
response channel changes from `mpsc::UnboundedSender<AgentResponse>` (single
consumer) to `broadcast::Sender<AgentResponse>` (multi-subscriber). This is the
enabling change for multiple clients subscribing to the same session's event
stream. Inbound requests stay `mpsc` (a single ordered queue the driver drains
FIFO); only the fan-out direction becomes broadcast.

### 4. Driver task is independent of the registry map

`SessionRegistry`'s `RwLock<HashMap>` is written only on `create_session` /
`close_session`. A running turn communicates purely over channels and never
touches the map, so session lookups never contend with turns. A close routes
through a `CancellationToken` clone held in the registry (see `TurnControl` in
`registry.rs`), not through the request channel.

### 5. The TUI coupling point is narrowed to one trait

The `/export` + clipboard path in `handlers/slash.rs` is the only thing that
prevents `handlers/` from being a clean move. It will be replaced with a small
trait (e.g. `trait UiBridge { async fn copy_to_clipboard(&self, text: &str); … }`)
that the frontend injects into the slash dispatcher. The TUI provides a real
impl; the server provides a no-op or "copy to a temp file" impl. This keeps the
slash handler frontend-agnostic without inventing a new abstraction for a
single call site.

### 6. Embed-or-serve: `agent_loop` stays on `mpsc`, not `broadcast`

The original design (§3) proposed switching the harness→frontend response
channel from `mpsc` to `broadcast` unconditionally. In practice the dominant
deployment is **single-instance standalone** (the TUI process is the only
client), with `neenee serve` (multi-session daemon) as the occasional case.

Forcing `broadcast` on the standalone path makes every `ToolResult` — which can
carry an entire file's contents — pay a `Clone` it does not need, and adds a
phantom second subscriber that never exists. The revised design keeps
`agent_loop::run` on `mpsc::UnboundedSender<AgentResponse>` unchanged:

- **Standalone mode (90%):** the TUI holds the `mpsc` receiver directly. Zero
  serialization, zero cloning, identical to pre-refactor behavior.
- **Serve mode (10%):** the WS bridge runs a thin fan-out task — `mpsc` →
  `broadcast` — that subscribes N WebSocket clients to one session's stream:

  ```text
  agent_loop::run  ──mpsc──►  fan-out task  ──broadcast──►  WS client 1
                                          └──────────────►  WS client 2
  ```

  `agent_loop` is unaware it is in serve mode; the fan-out task is the sole
  consumer of its `mpsc` output.

This means `mpsc`→`broadcast` (§3) is **not** a change to `agent_loop` at all.
It is a concern of the serve-mode transport bridge, applied only when a
session has multiple subscribers. The standalone path never touches
`broadcast`.

### 7. Process orchestration

```text
Mode::Standalone (default)        Mode::Serve (explicit)
┌───────────────────────┐         ┌────────────────────────┐
│  neenee-code process  │         │  daemon process        │
│                       │         │                        │
│  mpsc: req / resp     │         │  SessionRegistry       │
│  ┌─────────────────┐  │         │  ├ session #1          │
│  │ agent_loop::run │  │         │  │   agent_loop::run   │
│  │ (shared kernel) │  │         │  │   mpsc→broadcast    │
│  └────────┬────────┘  │         │  ├ session #2   …      │
│           │ mpsc      │         │  └ WS listener          │
│      TUI (direct)     │         └───────┬────────────────┘
└───────────────────────┘                 │ JSON over WS
                                  ┌───────┴────────────────┐
                                  │ browser / remote TUI   │
                                  └────────────────────────┘
```

Both modes share `agent_loop::run` and every handler — the entire driver
surface area is one code path. The TUI's standalone entry constructs a
one-element channel pair and calls `run` directly; the serve entry wraps each
session's `mpsc` output in a fan-out task that broadcasts to WebSocket
subscribers.

## Alternatives considered

- **Status quo: keep driving `neenee-agent` directly from `neenee-code`, build
  the web frontend as a second independent binary.** Rejected: it would
  duplicate the ~4000-line driver (`agent_loop` + `handlers` + side/view
  helpers) in `neenee-web`, and the two would drift on every dispatch/compaction/
  permission change. The whole point of the shared layer is to write that logic
  once.

- **Make `neenee-server` a standalone daemon binary (separate from
  `neenee-code`).** Rejected for now: the user explicitly chose the
  "server embedded in neenee-code" packaging. `neenee serve` becomes a
  subcommand of the existing binary; the TUI's current in-process mode stays a
  first-class path (a `neenee-server` lib the binary links and drives directly,
  no socket). A standalone `neenee-serverd` binary remains a future option if
  deployment later demands it, and requires zero API change (it would just be a
  `main.rs` consuming the same library).

- **Abstract only a "shared driver crate," no session registry.** Rejected:
  that is the A-proposal (shared driver, peer frontends, same single-session
  process model). It does not satisfy the stated goal (B: a long-running daemon
  managing multiple sessions). The registry + broadcast channel are the
  load-bearing additions; without them the daemon cannot exist.

- **Put the session registry inside `neenee-agent`.** Rejected: `neenee-agent`
  is the orchestration of a single `Agent`'s turn machinery. Multi-session
  lifecycle, broadcast fan-out, and transport are a different concern and would
  drag `neenee-agent` toward depending on concrete tools/providers (which the
  registry assembles per session), breaking the "agent depends only on core +
  store + providers" invariant from ADR-0005.

## Consequences

- **Positive.** A browser frontend (`neenee-web`) becomes reachable with no
  driver-logic duplication: it speaks the already-serializable
  `AgentRequest`/`AgentResponse` protocol over a transport this crate owns.

- **Positive.** Multiple clients can observe the same session — two browser
  tabs, or a TUI and a browser, subscribing to one session's stream. The
  `broadcast` channel makes this natural; the old `mpsc` made it impossible.

- **Positive.** The strict DAG is preserved. `cargo tree` for `neenee-code`
  gains one intermediate node; nothing reverses.

- **Negative (transient).** A ~4000-line code migration from `neenee-code` into
  `neenee-server` (`agent_loop`, `handlers/*`, `side`, `session_view`,
  `agent_setup`, `pursuits`, `review`, `shell`, `hooks`, `mcp_catalog`). This is
  the bulk of the remaining work; it is mechanical (the handlers are already
  parameterized over `&Agent` / `&Config` / channel handles, not over TUI
  types).

- **Negative (mild).** One TUI coupling point (`/export` + clipboard in
  `handlers/slash.rs`) must be abstracted behind a trait before the slash
  handler can move. Bounded — it is a single ~30-line call site.

- **Neutral.** The workspace grows from seven crates to eight. `neenee-code`'s
  `Cargo.toml` gains one path dep (`neenee-server`); `neenee-server`'s deps are
  the union of what the driver logic needs (agent + store + providers + tools +
  core).

## Migration mechanics

The work is staged so each step compiles and changes no runtime behavior until
the final wiring swap.

| Step | What | Status | Files |
|------|------|--------|-------|
| 0 | Make `AgentRequest`/`AgentResponse`/`TurnEvent`/`AgentEvent`/`AgentOp`/`SubagentEvent` + their DTOs `Serialize`/`Deserialize` (prerequisite for any transport). One field type change: `SubagentEvent::Started { profile: &'static str }` → `String` (`&'static str` cannot `Deserialize`). | ✅ Done | `crates/neenee-core/src/events.rs`, `tool_output.rs`, `pursuits/mod.rs`; 3 call sites in `subagent_tool.rs` / `tui/document.rs` |
| 1 | Scaffold `crates/neenee-server/` with `SharedState` (implemented), `SessionRegistry` + `SessionHandle` (skeletons with `create_session`/`close_session` TODOs). Add to workspace. | ✅ Done | `crates/neenee-server/{Cargo.toml,src/lib.rs,src/shared.rs,src/registry.rs}`, root `Cargo.toml` |
| 2 | Move the TUI-free driver modules from `neenee-code` into `neenee-server`: `agent_loop`, `handlers/*`, `side`, `session_view`, `agent_setup`, `pursuits`, `review`, `shell`, `hooks`, `mcp_catalog`, `export`, `startup`. | ✅ Done — 16 modules moved via `git mv` + `pub(crate) use` re-export. `neenee-code/src` now contains only `main.rs` + TUI. | `crates/neenee-server/src/*.rs`, `crates/neenee-code/src/main.rs` |
| 3 | Introduce `UiBridge` trait; replace the `/export` + clipboard call site in `handlers/slash.rs`; move `handlers/slash` + `export` + `startup` into `neenee-server`. | ✅ Done | `crates/neenee-server/src/{ui_bridge.rs,export.rs,startup.rs,handlers_slash.rs}`, `crates/neenee-code/src/tui/clipboard.rs` (`TuiClipboard` impl) |
| 4 | Move `agent_loop` (the dispatcher) into `neenee-server`. **Revised per §6:** `agent_loop::run` stays on `mpsc` (not `broadcast`); the fan-out is a serve-mode transport concern. The `UiBridge` is threaded through `Harness` as `Arc<dyn UiBridge>`. | ✅ Done | `crates/neenee-server/src/agent_loop.rs`, `crates/neenee-code/src/main.rs` |
| 5 | Hot-attach WebSocket transport: `/serve <port>` slash command intercepted in the TUI event loop (pure frontend concern — never reaches `agent_loop`). Creates a `broadcast::Sender`, stores it in `App::serve_tap`, spawns `serve::start_server`. The response listener taps each `AgentResponse` into the broadcast (zero cost when inactive — one `Option::is_none()` check). WS protocol: newline-JSON `Wire` envelope (`History` replay on connect + `Response` live stream out, `Request` in). | ✅ Done (prototype) | `crates/neenee-server/src/serve.rs`, `crates/neenee-code/src/tui/{app.rs,mod.rs,event_loop.rs}` |
| 6 | Populate `SessionRegistry::create_session` / `close_session` (port the `Harness` construction from `main.rs` into the registry). | Pending | `crates/neenee-server/src/registry.rs` |

## References

- ADR-0005 — strictly-layered topology. This ADR adds one layer between `agent`
  and the applications without disturbing the DAG.
- ADR-0017 — side conversations. Established that a second `Agent`+`SessionStore`
  peers the primary's turn state, proving per-session agents (and thus the
  registry's per-session `Agent` ownership model) are sound.
- ADR-0029 — full-duplex subagent communication. The `AgentRequest::PermissionReply`
  / `UserQuestionReply` + `parent_call_id` routing is unchanged; the server layer
  merely transports it.
- ADR-0035 — application-layer split. `neenee-server` is the natural shared sink
  beneath `neenee-code` and a future `neenee-web`.
- `crates/neenee-core/src/events.rs` — the now-serializable harness↔driver
  protocol.
- `crates/neenee-server/src/lib.rs` — the three-abstraction overview and
  migration posture.
