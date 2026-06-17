# neenee — Agent-focused Project Guide

This file contains the background, architecture, and conventions that coding
agents need to work effectively in the neenee repository.

> **Project:** neenee — A Rust-based interactive AI coding agent with a
> semantic TUI, native tool use, and a skill system inspired by opencode.

---

## 1. Architecture Overview

```
┌─────────────┐    mpsc channels   ┌─────────────────────────────┐
│ neenee      │◄──────────────────►│ neenee-core                 │
│ (launcher)  │                    │ • Agent (ReAct loop)        │
└─────────────┘                    │ • Tool registry             │
       │                           │ • Skills system             │
       ▼                           │ • Providers (OpenAI, ...)   │
┌─────────────────────────────┐    └─────────────────────────────┘
│ neenee-tui                  │
│ • Semantic document model   │
│ • LayoutMap / Selection     │
│ • Slash cmd autocomplete    │
└─────────────────────────────┘
```

### Crate responsibilities

| Crate | Responsibility |
|-------|---------------|
| `neenee-core` | **The brain.** Agent loop, tool definitions, skill discovery, provider abstractions. Must stay UI-agnostic. |
| `neenee-tui` | **The face.** Ratatui-based terminal UI with semantic selection (not character-grid selection). Owns `document`, `layout`, `selection`, `render`, `input`, `clipboard`. |
| `neenee` | **The launcher.** Wires core + TUI, spawns the agent background task, handles provider switching, slash commands, config persistence. |

There is intentionally no daemon/server crate: the CLI process owns the agent
background task and talks to the TUI over in-process mpsc channels. If
headless or remote use is ever needed, wrap `neenee-core` at that point
rather than reintroducing a gRPC layer ahead of a concrete need.

---

## 2. Key Design Decisions

### 2.1 Semantic TUI (opentui-style selection)

The TUI does **not** rely on the terminal emulator's character-grid selection.
Instead:

1.  Every chat message is stored as a **structured document model**
    (`ChatMessage` → `Block` variants: `Text`, `Code`, `Heading`, `Quote`,
    `ListItem`, `Rule`, `Break`).
2.  During rendering we record each block's screen coordinates in a
    **`LayoutMap`** (`BlockRegion` with byte offsets).
3.  Mouse events are resolved through `LayoutMap.hit_test()` back to a
    **`SemanticCursor`** (`message_idx`, `block_idx`, `byte_offset`). Each
    `BlockRegion` carries the exact rendered line text plus its decoration
    prefix width; the column→byte mapping walks the text by Unicode display
    width, so multi-byte and wide (CJK) characters resolve to correct char
    boundaries. The selection head character is inclusive, offsets are
    boundary-snapped, and the selected range is highlighted per character
    while dragging.
4.  Copying calls `selection::get_selected_text()` which extracts the
    **original raw text** from the document model — never the terminal-wrapped
    display characters.
5.  Clipboard integration uses **OSC52** (works over SSH/tmux) plus the
    system clipboard. Wayland prefers `wl-copy` so clipboard ownership remains
    alive after the copy call; `arboard` and OSC52 are fallbacks. The TUI must
    wait for the copy result before reporting success.

This is the single most important invariant of the TUI layer. If you change
rendering, you must keep `LayoutMap` accurate.

Markdown is parsed with `pulldown-cmark` into semantic blocks during both
streaming and final rendering. Every stream delta reparses the accumulated
message so headings, paragraph boundaries, quotes, fenced code, ordered,
unordered and task lists, and basic tables do not appear only at `StreamEnd`.
The live and completed block trees must remain equivalent for identical raw
content.

Markdown soft breaks render as spaces, while explicit hard breaks remain line
breaks. Terminal wrapping applies basic CJK kinsoku rules so closing punctuation
does not begin a visual line and opening punctuation does not end one.

The header shows only durable harness state: the neenee brand, the active
provider/model, the mode (Build/Plan), and the current goal with checklist
progress. Transient running status is **not** in the header; it is rendered
inline at the end of the neenee message stream as a transient
`┃ neenee ⟳ <status>` line. Input submission reports `queued`, followed by
request persistence, context preparation/compaction, model wait, response
finalization, and response persistence. Loop progress remains visible while
tool events report `exploring`, `searching codebase`, `making edits`,
`running command`, `using MCP`, or permission wait. The status line is hidden
while assistant text is actively streaming (the streamed text is itself the
feedback) and hidden when idle.

The chat view follows the newest content automatically. Scrolling up pauses
follow; scrolling back to the bottom (or sending a message) re-engages it, so
the inline status line and streamed responses stay visible.

### 2.2 Dual-path Tool Calling

Not all providers support native function calling. We support both paths:

- **Native** — OpenAI-compatible APIs receive `tools` + `tool_choice: auto`.
  The provider parses `tool_calls` from the response JSON.
- **Universal fallback** — All providers (including Gemini and local Llama)
  can emit tool calls as plain text in the format:
  ```json
  {"tool": "read_file", "arguments": {"path": "src/main.rs"}}
  ```
  `Agent::parse_tool_call()` extracts this and routes it through the same
  execution pipeline.

When adding a new provider, you only need to implement `Provider::chat()`;
if it does not support native tools, the fallback automatically works.

### 2.3 Two-phase Skill Loading (opencode-inspired)

Skills are markdown files with YAML frontmatter. To avoid wasting context on
unused skills:

1.  **Discovery phase** — At startup we scan `.neenee/skills/` and
    `~/.neenee/skills/`, parse *only* the frontmatter, and build a compact
    index embedded in the system prompt.
2.  **Load phase** — The agent decides a skill is relevant and calls
    `use_skill(name)`. The full markdown body is then injected into the
    conversation as a `Tool` result.

Priority order (highest wins):
1. `./.neenee/skills/*.md`
2. `~/.neenee/skills/*.md`

### 2.4 MCP tool discovery

Local stdio MCP servers are configured under `[mcp.<name>]`. At CLI startup,
`neenee-core::mcp` initializes each enabled server, calls `tools/list`, and
adapts every definition to the existing `Tool` trait.

Public tool names use `mcp__<server>__<tool>` so tools from different servers
cannot collide. MCP calls therefore receive the same Plan-mode checks, ReAct
round limits, event projection, and TUI rendering as built-in tools.

Each server has an 8-second startup timeout. `/mcp` shows `connected`,
`disabled`, or `failed` status without preventing the rest of the application
from starting.

### 2.5 Custom slash commands

Reusable prompt commands are discovered from `.neenee/commands/*.md` and
`~/.neenee/commands/*.md`, with project-local definitions taking priority.
YAML frontmatter may define `name` and `description`; otherwise the filename
is the command name.

Templates support `$ARGUMENTS` plus positional `$1` through `$9`. Invocations
use the normal cancellation, retry, permission, event, compaction, and session
path. The expanded prompt is sent to the provider while the original slash
invocation remains the display text for TUI and session replay. Built-in
command names are reserved.

---

## 3. Tool System

Tools live in `crates/neenee-core/src/tools.rs`. Each tool implements the
`Tool` trait:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    fn access(&self) -> ToolAccess { ToolAccess::Write }
    async fn call(&self, arguments: &str) -> Result<String, String>;
    async fn call_with_events<'a>(
        &self,
        _call_id: &str,
        arguments: &str,
        _on_event: Box<dyn FnMut(SubTaskEvent) + Send + 'a>,
    ) -> Result<String, String> {
        self.call(arguments).await
    }
    fn to_openai_function(&self) -> serde_json::Value;  // auto-generated default
}
```

### Available tools

| Tool | Key parameters | Safety note |
|------|---------------|-------------|
| `read_file` | `path`, `offset`, `limit` | Large files (>8000 chars) are truncated with a preview |
| `write_file` | `path`, `content` | Creates parent directories automatically |
| `edit_file` | `path`, `old_string`, `new_string` | Requires exact match; falls back to whitespace-normalized match |
| `bash` | `command`, `timeout` | Large outputs are truncated |
| `grep` | `pattern`, `path`, `ext` | Uses `rg`; capped at 50 matches |
| `glob` | `pattern`, `path` | Fast file pattern matching (`**/*.rs`); skips VCS/build dirs |
| `list_dir` | `path`, `pattern`, `recursive`, `max_results` | Respects `.git`/build dirs via ignore patterns |
| `webfetch` | `url`, `raw` | Fetches a URL; HTML is stripped to text; output truncated |
| `websearch` | `query` | DuckDuckGo search (no API key); best-effort |
| `todo` | `items[]` | Standalone in-process task list; read-only for permissions |
| `task` | `description`, `prompt` | Spawns a read-only exploration sub-agent; no recursion |
| `create_project` | `name`, `type`, `path`, `git`, `neenee` | Scaffolds rust/node/python/go/generic projects |
| `init_config` | `path` | Idempotently creates a `.neenee/` tree |
| `use_skill` | `name` | Loads skill content into context |
| `goal_checklist` | `items[]` | Replaces the active goal checklist; internal harness state, no permission prompt |
| `mcp__<server>__<tool>` | Server-defined JSON schema | Dynamically discovered from configured local stdio MCP servers |

### Adding a new tool

1.  Create a unit struct in `crates/neenee-core/src/tools.rs`.
2.  Implement `Tool` with `#[async_trait]`.
3.  Register it in `neenee/src/main.rs` inside the `tools` vector. The
    harness-owned `goal_checklist` tool is registered internally by `Agent`.
4.  If it is a **write** tool, add it to the Plan-mode blocklist in
    `Agent::execute_tool()`.

---

## 4. Agent Run Loop

`Agent::run()` is a ReAct-style loop:

```
1. Inject / update system prompt (tools + skills index + current mode)
2. Call provider.chat(messages)
3. If response contains tool_calls (native or parsed):
      a. Execute each tool
      b. Push tool results as Role::Tool messages
      c. GOTO 2
4. If no tool calls, return the final assistant message
```

### Agent Mode (Build vs Plan)

The agent operates in one of two modes, switchable at runtime via `/mode`:

| Mode | Behavior |
|------|----------|
| `Build` | Full read/write tool access. The agent can create, modify, and delete files. |
| `Plan` | Only tools explicitly marked `ToolAccess::ReadOnly` may run. Write-capable and unclassified tools are blocked. |

Mode state lives in `Agent.mode: Mutex<AgentMode>`. The system prompt is
re-injected on every turn so the LLM always sees the current mode.

### Harness safety and execution

Interactive turns go through `Agent::run_streaming_with_events()`. OpenAI
compatible tool-call deltas are reassembled by index before execution, while
providers without native tools keep the universal JSON fallback. Both paths
use the same ReAct limits, permission checks, persistence, and TUI events.
Fallback JSON is removed from the visible transcript before its tool card is
shown.

The ReAct loop has two hard safety bounds:

- At most 32 tool rounds per agent turn.
- At most 3 consecutive identical tool calls.

Tool calls and results are emitted as `AgentEvent` values and projected into
the TUI as semantic tool messages.

### Goal and autonomous loop

`Agent` owns an optional `Goal` that is injected into the system prompt on
every turn. A goal is `Active` or `Completed`.

Goals may contain a structured checklist with `pending`, `in_progress`,
`completed`, or `cancelled` items. The built-in `goal_checklist` tool replaces
the list, allows at most one in-progress item, persists changes through the
CLI, and updates the TUI header immediately. It is classified read-only for
permission purposes because it changes only harness metadata. Once populated,
an active checklist cannot be cleared; items must be completed or cancelled.

`/loop <N>` runs the active goal for at most `N` autonomous iterations
(`1..=50`). Each iteration is a normal, fully tooled agent turn. The model may
finish early by emitting `[NEENEE_GOAL_COMPLETE]`; the marker is removed from
the displayed response and the goal becomes `Completed`. If a checklist
exists, the marker is accepted only when every item is completed or cancelled.

`/loop resume` restores an unfinished durable checkpoint after a process
restart, provider error, or interruption. It retries the unfinished iteration
and removes a trailing hidden loop control prompt left by interrupted
admission. Completed and exhausted checkpoints are terminal.

The loop is cancellable through `/loop stop`, `Esc`, or a newer chat/loop
request. The CLI uses a generation id so an older task cannot clear or cancel
the control state of a newer task.

### Tool permissions

Build-mode tools marked `ToolAccess::Write` require an interactive decision
before execution:

- **Allow once** executes only the pending call.
- **Always allow** requires a second confirmation and caches the tool plus
  resource scope for the current process. File tools scope by path and bash
  scopes by the complete command.
- **Reject** returns a denied tool result to the model.

Read-only tools do not prompt. Plan mode blocks write tools before permission
evaluation. Interrupting or superseding a task rejects all pending requests.
`/permissions` lists process-local always rules and `/permissions clear`
revokes them.

### Sub-agents (the `task` tool)

The `task` tool lets the main agent delegate a focused research or exploration
sub-task to a read-only sub-agent, mirroring opencode's explore-agent pattern:

1.  `TaskTool` is constructed in `neenee/src/main.rs` with a **snapshot** of
    the parent agent's toolset plus the live provider. Because the snapshot is
    taken *before* the task tool is added to the list, the sub-agent can never
    see the `task` tool — recursion is structurally impossible.
2.  At call time the snapshot is filtered to `ToolAccess::ReadOnly` tools only,
    so the sub-agent can `read_file`, `grep`, `glob`, `list_dir`, and `webfetch`
    but cannot mutate the workspace and **never** prompts for permission.
3.  The sub-agent runs through the normal `Agent::run()` ReAct loop (headless,
    bounded by the same 32-round limit) with a focused system prompt.
4.  Only the final written answer is returned to the parent agent, which stays
    in control of any edits.

Use `task` to parallelize investigation (where code lives, summarizing files,
gathering web context). The parent performs writes after reviewing findings.

---

## 5. Provider Implementations

All providers are in `crates/neenee-core/src/providers.rs`.

| Provider | Base URL | Notes |
|----------|----------|-------|
| `OpenAIProvider` | `https://api.openai.com/v1/chat/completions` | Full native tool support via `prepare_tools()`. Sends `tools` array in request body. |
| `GeminiProvider` | `https://generativelanguage.googleapis.com/v1beta/...` | Uses Gemini REST API. No native tools yet; relies on universal fallback. Harness context is sent through `systemInstruction`. |
| `LlamaServerProvider` | configurable (default `http://localhost:8080`) | OpenAI-compatible local server (llama.cpp, vLLM, etc.). |
| `KimiCodeProvider` | `https://api.kimi.com/coding/v1/chat/completions` | Kimi Code subscription API. Env: `KIMI_CODE_API_KEY`. The request model ID is always `kimi-for-coding`; Kimi maps it to the latest coding model. Uses the approved OpenCode-compatible User-Agent, overridable through `KIMI_CODE_USER_AGENT`. |
| `KimiProvider` | `https://api.moonshot.cn/v1/chat/completions` | Kimi Open Platform wrapper. Env: `KIMI_API_KEY`. Models: `moonshot-v1-8k`, `moonshot-v1-32k`, etc. |
| `DeepSeekProvider` | `https://api.deepseek.com/v1/chat/completions` | OpenAI-compatible wrapper. Env: `DEEPSEEK_API_KEY`. Models: `deepseek-chat`, `deepseek-reasoner`. |
| `QwenProvider` | `https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions` | OpenAI-compatible wrapper. Env: `DASHSCOPE_API_KEY`. Models: `qwen-plus`, `qwen-max`, `qwen-coder-plus`. Also has `new_intl()` for international endpoint. |
| `GLMProvider` | `https://open.bigmodel.cn/api/paas/v4/chat/completions` | OpenAI-compatible wrapper. Env: `GLM_API_KEY`. Models: `glm-4-plus`, `glm-4`, `glm-4-air`. |
| `VolcengineProvider` | `https://ark.cn-beijing.volces.com/api/v3/chat/completions` | OpenAI-compatible wrapper. Env: `VOLCENGINE_API_KEY`. Models: `deepseek-v3-250324`, `doubao-pro-256k`. |
| `MockProvider` | — | Returns canned responses for testing. |

### OpenAI-compatible wrapper pattern

Kimi Code, Kimi Open Platform, DeepSeek, Qwen, GLM, and Volcengine are thin wrappers around
`OpenAIProvider::with_base_url()`. They inherit full tool calling + streaming.
When adding another OpenAI-compatible service, just create a new-typed struct
wrapping `OpenAIProvider` and delegate all three trait methods.

---

## 6. Slash Commands & Autocomplete

The input box supports slash commands. When the user types `/`, a popup
appears above the input line showing matching commands with descriptions.
Discovered custom commands are included in the same popup.

### Commands

| Command | Action |
|---------|--------|
| `/models` | Open the provider selection modal (same as `Ctrl+M`) |
| `/mode [build\|plan]` | Show or switch agent mode. Supports autocomplete for subcommands (`build`, `plan`). |
| `/mcp` | Show MCP server connection status and discovered tool count. |
| `/permissions [clear]` | Show or clear process-local always-allowed tools. |
| `/session status` | Inspect the active durable session. |
| `/session list` | List durable session branches using short ids. |
| `/resume [id]` | Resume the most recent cached session, or a session matching a unique id prefix. |
| `/session resume [id]` | Namespaced alias for `/resume`. |
| `/session fork` | Fork the current transcript into a new active session. |
| `/session open <id>` | Open a session by unique id prefix. |
| `/session new` | Archive the current branch and start an empty session. |
| `/compact` | Compact older complete turns into a durable checkpoint. |
| `/goal <objective>` | Set the persistent goal. Also supports `status`, `done`, and `clear`. |
| `/loop <1-50>` | Run bounded autonomous iterations. Also supports `resume`, `status`, and `stop`. |
| `/init [path]` | Initialize a `.neenee/` configuration tree (skills, commands, agents) in the target directory. |
| `/clear` | Clear the conversation history (keeps system prompt) |
| `/help` | Display available commands |
| `/exit` | Gracefully exit the program |
| `/<custom> [args]` | Expand a project/user command template and run it through the agent harness |

### Autocomplete behavior

- Typing `/` triggers the suggestion popup.
- `↑` / `↓` navigate suggestions when visible.
- `Tab` cycles through and accepts suggestions.
- `Enter` with suggestions visible auto-selects the first match.
- `Esc` closes the popup.
- Subcommand completion works for `/mode `, `/goal `, and `/loop `.

The popup is a ratatui `List` inside a rounded `Block` with title
"Commands" and a bottom hint bar.

---

## 7. Modals

The TUI modal states (`App.active_modal`):

| Modal | Trigger | Navigation | Action |
|-------|---------|------------|--------|
| **Models** | `/models` or `Ctrl+M` | `↑`/`↓` | Select a preset solution or configure a custom OpenAI-compatible relay. Presets pair provider, model, and key scope. |
| **History Search** | `Ctrl+R` | `↑`/`↓` | `Enter` inserts selected history item into input |
| **API Key** | `k` in Models modal | — | Type the key (masked); `Enter` saves to config and switches provider; `Esc` cancels and restores the stashed input line |
| **Endpoint / Model** | Custom relay | — | Enter the full chat-completions endpoint and model ID before the API-key step. |
| **Permission** | Write tool call | `↑`/`↓` | `Enter` confirms the selected decision |

All modals are centered overlays with a border. `Esc` closes any modal. While
the API-key modal is open it borrows the input line; the previous input is
stashed and restored on close.

---

## 8. Configuration & Persistence

Config is stored in `~/.config/neenee/config.toml` (managed by
`neenee/src/config.rs`).

### Supported fields

```toml
default_provider = "openai"
openai_api_key = "sk-..."
openai_model = "gpt-4o"
gemini_api_key = "..."
gemini_model = "gemini-1.5-flash"
llama_base_url = "http://localhost:8080"
llama_model = "local-model"
kimi_code_api_key = "..."
kimi_code_user_agent = "opencode/1.17.4"
kimi_api_key = "..."
kimi_model = "moonshot-v1-8k"
custom_api_key = "..."
custom_model = "relay-model-id"
custom_base_url = "https://relay.example/v1/chat/completions"
deepseek_api_key = "..."
deepseek_model = "deepseek-chat"
qwen_api_key = "..."
qwen_model = "qwen-plus"
glm_api_key = "..."
glm_model = "glm-4-plus"
volcengine_api_key = "..."
volcengine_model = "deepseek-v3-250324"
compaction_max_chars = 120000
compaction_preserve_turns = 6
provider_retry_max_attempts = 4
provider_retry_base_ms = 1000
provider_retry_max_ms = 30000

[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."]
enabled = true
read_only = false

[mcp.filesystem.environment]
# OPTIONAL_VARIABLE = "value"
```

MCP tools default to write-capable and are blocked in Plan mode. Set
`read_only = true` only when every tool exposed by that server is known to be
read-only.

Environment variables override config values (e.g. `OPENAI_API_KEY` >
`config.openai_api_key`). Custom relay overrides are `CUSTOM_API_KEY`,
`CUSTOM_MODEL`, and `CUSTOM_BASE_URL`. The Kimi Code compatibility identity
can be overridden with `KIMI_CODE_USER_AGENT`.

API keys can be entered interactively: in the Models modal, press `k` on a
provider to open the masked key-entry modal. The key is persisted to
`config.toml` via `AgentRequest::SwitchProvider { api_key, .. }` and the
provider is switched immediately. Environment variables still win at runtime.

### Input history

Input history is persisted to `~/.config/neenee/history.json` on exit and
restored on startup. Navigate with `↑`/`↓` when the input box is empty.

### Durable session

The active conversation is persisted to `~/.config/neenee/session.json`.
Branch snapshots live under `~/.config/neenee/sessions/<id>.json`.
User input is admitted to disk before the provider call; completed message and
tool history is committed again after the turn. Startup archives the previous
active conversation and creates a fresh empty session while retaining the
last provider/model configuration. `/resume` restores the most recent cached
session; `/resume <id-prefix>` selects a specific one.

`/session fork` archives the source and creates a child session with the same
transcript and compaction state but no running loop checkpoint. `/session list`
shows branches and `/session open <id-prefix>` switches both model history and
the visible semantic transcript. Turns are bound to their admission session id
so a late provider result cannot commit across a session switch. Goal state is
project-level harness state and is intentionally not forked.

Loop checkpoints store the goal, current/max iteration, and terminal status
(`running`, `completed`, `interrupted`, `error`, or `exhausted`).
`/session status` inspects this state and `/session new` cancels active work,
clears the TUI, and creates a new session id.

### Context compaction

Before a provider turn, the CLI estimates model-visible request size by UTF-8
characters. When it exceeds `compaction_max_chars`, complete older user turns
move to the durable archive. Active model history receives a hidden
deterministic checkpoint plus the most recent `compaction_preserve_turns`.

`/compact` triggers the same operation manually. A provider context-overflow
error receives one compact-and-retry attempt only when the failed physical
attempt emitted no tool call, preventing side-effect replay.

### Provider retry

HTTP 408, 429, 5xx, connection failures, and timeouts are tagged as retryable.
The CLI respects `Retry-After`/`retry-after-ms`, otherwise using bounded
exponential backoff. Retry waits are visible in the TUI and cancellable with
`Esc`. Partial streamed text is discarded before retry. Automatic retry stops
after the configured attempt limit (hard-capped at 10) and is never performed
after any tool call event, preventing tool side-effect replay.

---

## 9. Interaction & Key Bindings

| Key | Context | Action |
|-----|---------|--------|
| `Enter` | Normal | Send message or execute slash command |
| `Enter` | Suggestions visible | Accept first suggestion |
| `Tab` | Suggestions visible | Cycle through suggestions |
| `↑`/`↓` | Normal (empty input) | Navigate input history |
| `↑`/`↓` | Suggestions visible | Navigate suggestions |
| `↑`/`↓` | Modal open | Navigate modal items |
| `←`/`→` | Normal | Move cursor in input box |
| `Backspace` | Normal | Delete character before cursor |
| `Esc` | Modal open | Close modal |
| `Esc` | Responding | Interrupt current generation |
| `Ctrl+M` | Normal | Open models modal |
| `Ctrl+R` | Normal | Open history search modal |
| `Ctrl+T` | Normal | Expand or collapse all semantic tool steps |
| `Ctrl+Shift+C` / `Cmd+C` | Any | Copy current semantic selection |
| `Ctrl+C` | Selection active | Copy current semantic selection |
| `Ctrl+C` | Responding | Interrupt current generation |
| `Ctrl+C` | Modal open (except Permission) | Close modal |
| `Ctrl+C` | Non-empty input | Clear input line |
| `Ctrl+C` ×2 | Empty input, idle | Quit (second press within ~2s) |
| `k` | Models modal | Configure API key for highlighted provider |
| `q` | Empty input | Quit |
| Mouse drag | Chat area | Semantic text selection |
| Mouse middle-click | Chat area | Select entire block |
| Mouse scroll | Any | Scroll chat |

---

## 10. Coding Style

- **Rust edition 2021**, resolver 2.
- Use `cargo check` and `cargo fix` regularly.
- Keep `neenee-core` UI-agnostic. No ratatui/crossterm code in core.
- Prefer `Arc<dyn Trait>` for polymorphic components (Provider, Tool).
- Use `tokio::sync::Mutex` (not `std::sync::Mutex`) when the guard crosses
  an `.await` boundary.
- **Document model** (`ChatMessage`) owns the raw text; `Block` owns the
  semantic slice. Rendering never mutates the document model.

---

## 11. Build

```bash
cargo build
cargo run            # neenee is the workspace's only binary
```

No special build steps and no protobuf toolchain required.

---

## 12. When You Change Something

| If you change... | Also update... |
|------------------|----------------|
| Add/remove a tool | `neenee/src/main.rs` tool list; this AGENTS.md tool table |
| Add a project template | `project.rs` scaffold functions; this AGENTS.md tool table |
| Change `Tool` trait | All provider impls; `to_openai_function()` default may suffice |
| Change TUI rendering | Ensure `LayoutMap` regions remain accurate per block |
| Change skill format or discovery | `skills.rs` + this AGENTS.md section 2.3 |
| Change custom command format or discovery | `commands.rs` + this AGENTS.md section 2.5 |
| Change `AgentResponse` variants | `neenee-tui/src/lib.rs` response handler match arms |
| Add a new provider | `providers.rs`, `neenee/src/main.rs` match arms, `config.rs`, this AGENTS.md provider table |
| Add a slash command | `SLASH_COMMANDS` in `neenee-tui/src/lib.rs`, handler in `neenee/src/main.rs`, this AGENTS.md command table |

---

## 13. Quick Reference

**Skill file template:**
```markdown
---
name: my-skill
description: "Use when ..."
---
# My Skill
Instructions here.
```

**Tool result format in conversation:**
```
Role::Tool  →  "[tool_name result]:\n<output>"
```

**Semantic cursor:**
```rust
SemanticCursor { message_idx, block_idx, byte_offset }
```

**Add an OpenAI-compatible provider (copy-paste pattern):**
```rust
pub struct NewProvider(OpenAIProvider);
impl NewProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self(OpenAIProvider::with_base_url(api_key, model, "https://..."))
    }
}
#[async_trait]
impl Provider for NewProvider {
    fn prepare_tools(&self, tools: &[Arc<dyn Tool>]) { self.0.prepare_tools(tools); }
    async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> { self.0.chat(messages).await }
    async fn stream_chat(&self, messages: Vec<Message>) -> Result<BoxStream<'static, Result<String, String>>, String> { self.0.stream_chat(messages).await }
}
```
