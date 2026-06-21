# Sub-agents

The `task` tool spawns an isolated, read-only child agent to investigate a
sub-question and return a written answer. The parent agent stays in control of
all writes. This page covers the isolation model, event streaming, and the TUI
zoom view. For the `Tool` trait and access classes, see
[Built-in tools](../../reference/tools.md).

## Why a sub-agent tool

A single agent turn accumulates context: every file read, every grep result,
every tool round stays in the transcript. For a large investigation that
touches many unrelated corners of the codebase, one of two things happens —
either the context fills with material only loosely related to the final
answer, or the model spends turns re-reading things it already saw. A
sub-agent gives the model a way to delegate the exploration:

1. **Context isolation.** The sub-agent runs with a fresh two-message history
   (system + task prompt). Its tool rounds never enter the parent's transcript;
   only its final summary does.
2. **Read-only by construction.** The sub-agent receives only `Read` tools, so
   it cannot mutate the workspace, and it never triggers the permission
   broker.
3. **Parallelizable investigation.** The model can dispatch several `task`
   calls to map different parts of a problem, then act on the synthesized
   findings.

## The `task` tool

`TaskTool` (`crates/neenee-agent/src/task_tool.rs:24`) is the only built-in
tool that overrides the streaming entry points of the `Tool` trait. It lives
in the `neenee-agent` crate, not `neenee-tools`, because it constructs an
`Agent` internally — spawning a sub-agent is an orchestration concern.

| Member | Value |
|--------|-------|
| `name()` | `"task"` |
| `access()` | `ToolAccess::Read` |
| Parameters | `description` (string), `prompt` (string), both required |
| Overrides | `call_with_events`, `call_structured_with_events` |

The harness invokes `call_structured_with_events`
(`crates/neenee-agent/src/agent.rs:1582`), which runs the sub-agent and
returns a `ToolOutput::Subagent { summary, messages, usage }`
(`crates/neenee-agent/src/task_tool.rs:107`). The `messages` field is the full
child transcript; the parent persists it as the tool step's `children` so
`/resume` rebuilds the nested view.

Input validation (`task_tool.rs:131`) rejects only non-JSON or
empty-after-trim fields. The `<=60 chars` note in the parameter description is
a model-facing hint, not an enforced bound.

## Isolation model

The sub-agent shares exactly one thing with the parent — the provider — and
nothing else:

| Concern | Shared? | How |
|---------|---------|-----|
| Provider | Yes | `self.provider.clone()` (`task_tool.rs:159`) |
| Conversation history | No | Fresh `[System, User]` (`task_tool.rs:173`) |
| Tools | Snapshot, filtered | Read-only subset, see below |
| Goal state | No | `GoalStore::open_in_memory()` (`task_tool.rs:153`) |
| Plan state, mode | No | Sub-agent constructed with `AgentMode::Build` (`task_tool.rs:161`) |
| Skills | No | `SkillRegistry::empty()` (`task_tool.rs:163`) |
| Cancellation token | No | Fresh `CancellationToken::new()` (`task_tool.rs:193`) |
| Session persistence | No | Sub-agent's `Agent` is built without persistence |

The filesystem is implicitly shared because the sub-agent inherits the process
working directory, but its toolset has no write tools, so it cannot mutate
files.

### Tool filtering

The sub-agent receives the filtered intersection
(`crates/neenee-agent/src/task_tool.rs:146`):

```rust
let sub_tools: Vec<Arc<dyn Tool>> = self
    .tools
    .iter()
    .filter(|tool| tool.access() == ToolAccess::Read && tool.name() != "task")
    .cloned()
    .collect();
```

Two consequences fall out of the filter alone:

- **Recursion is impossible.** `task` is excluded, so a sub-agent cannot spawn
  another sub-agent.
- **Goal, plan, and verify tools are absent.** They are added inside the
  sub-agent's own `Agent::new`, but from a snapshot of the filtered set, and
  they share the sub-agent's own state cells — not the parent's. They are
  inert for a read-only research task.

The snapshot `TaskTool` holds is captured at construction
(`crates/neenee-cli/src/main.rs:449`), after built-ins and MCP tools are
assembled but before `SearchHistoryTool` is pushed. So MCP read-only servers
are visible to sub-agents; the history tool is not.

## Event streaming

The sub-agent is a real `Agent`, so it emits the full `AgentEvent` stream.
`TaskTool` translates each into a `SubTaskEvent`
(`crates/neenee-core/src/events.rs:225`) and forwards it, so the parent TUI
builds the nested view in real time:

```text
sub-agent AgentEvent ──forward_event──► SubTaskEvent
                                            │
parent dispatch wraps ──► AgentEvent::SubTask { parent_call_id, event }
                                            │
orchestration relay ──► AgentResponse::SubTask
                                            │
TUI appends to the matching tool step's children
```

`SubTaskEvent` carries the same shapes the parent stream does — `StreamStart`,
`StreamDelta`, `StreamEnd`, `ToolCall`, `ToolResult`, `Activity` — so the
zoomed view renders through the same transcript pipeline as the top-level
conversation. `forward_event` (`task_tool.rs:232`) ignores parent-only events
like `GoalUpdated`, `ModeChanged`, and `PermissionRequest` that have no
read-only-researcher meaning.

The `parent_call_id` is the dispatch-generated call id, not the model's
`call.id`, because the TUI keys its step off the `ToolCall` event id
(`crates/neenee-agent/src/agent.rs:1568`).

## TUI zoom view

The `task` step renders inline as one summary line plus a live status line
(`crates/neenee-cli/src/tui/render/step/renderers.rs:1004`). Pressing `Enter`
on the step — or clicking it — pushes onto the app's `focus_stack`
(`crates/neenee-cli/src/tui/app.rs:626`) and the transcript switches to
showing that step's children.

When zoomed in (`crates/neenee-cli/src/tui/render/mod.rs:204`):

- The entire footer — status bar, plan panel, input box, hint bar — is hidden.
  The sub-agent view is read-only chrome.
- A one-row navigation band at the bottom shows `Task <description> (N of M)`
  on the left and `Esc back   [ prev   ] next` on the right
  (`crates/neenee-cli/src/tui/render/step/renderers.rs:1111`).
- `Esc` pops the focus stack (`app.rs:633`); `[` and `]` cycle sibling `task`
  steps at the current depth (`app.rs:645`).
- The plan progress panel is hidden, because the plan belongs to the parent
  context (see [Plan mode](plan-mode.md)).

On `/resume`, persisted `Message::children` repopulate the step's children, so
the zoom view rebuilds from disk. The live event stream always wins over the
snapshot.

## Failure and cancellation

A sub-agent that hits a harness safety bound (32 tool rounds, three identical
calls) or a provider error still returns a `Subagent` payload
(`crates/neenee-agent/src/task_tool.rs:206`). Its `summary` is prefixed with
`Error:` so the existing failure classifier and the TUI's red Failed badge
both trigger, and the partial transcript is preserved so the user can resume
into the half-finished work. Only input-validation errors (bad JSON, missing
fields) propagate as `Err`, because they have no partial transcript worth
keeping.

The sub-agent runs with its own never-cancelled `CancellationToken`
(`task_tool.rs:193`). When the parent turn is interrupted, the parent's
dispatch drops the sub-agent future and emits `ToolCancelled` for the `task`
call id; the TUI then recursively cancels the nested tool steps
(`crates/neenee-cli/src/tui/document.rs`). The sub-agent does not need a token
linked to the parent — the parent simply stops awaiting it.

Real `TokenUsage` from the sub-agent is accumulated into the parent turn's
`TurnState` (`crates/neenee-agent/src/agent.rs:1175`) so cost flows up to the
active [goal](goals.md) if one is set.

## Plan mode

`task` is `Read`, so the default `allowed_in_plan_mode` permits it
(`crates/neenee-core/src/capability.rs:90`). The Plan-mode system prompt
explicitly endorses it as a read-only research tool.

The sub-agent is hardcoded to `AgentMode::Build` regardless of the parent's
mode. This is not a tension: the Plan-mode gate only matters for `Write`
tools, and the sub-agent has none. Whether the parent is in `Plan` or `Build`,
the sub-agent behaves as a read-only researcher.

## Related: `verify_plan_execution`

`VerifyPlanExecutionTool` (`crates/neenee-agent/src/plan_verify.rs:27`)
reuses `TaskTool` internally to spawn an independent verifier with a clean
context. It uses the plain `call` path rather than the structured streaming
path, so its nested step does not stream sub-agent tokens live — by design,
since a verifier reports a final PASS / PARTIAL / FAIL verdict. See
[Plan mode](plan-mode.md).

## See also

- [Built-in tools](../../reference/tools.md) — `task` parameter schema and access
- [Plan mode](plan-mode.md) — `task` in Plan mode, and `verify_plan_execution`
- [Tool rounds](tool-rounds.md) — the round trip the sub-agent runs internally
- [Goals](goals.md) — how sub-agent token cost flows up to a parent goal
- [Harness architecture](harness.md) — the safety bounds that bound a
  sub-agent turn
