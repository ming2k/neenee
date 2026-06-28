# Glossary

Canonical terms used across the neenee documentation. Each entry links to
its primary explanation or decision record. Where a term names a code
symbol, the symbol is backticked and never abbreviated.

## Execution model

| Term | Definition |
|------|------------|
| **turn** | The unit the user perceives: one submitted message and one final reply. Opens on submit, closes when the agent emits a final assistant message carrying no tool call. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **round** | One pass through the ReAct loop inside a turn: one model request plus the tool work that follows. The round counter resets every turn. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **turn counter** | A separate monotonic counter that persists across turns, for concerns that measure passage between turns (plan staleness, pursuit accounting). [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **ReAct loop** | The model-request → tool-call → result loop a round iterates on. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **harness** | The control plane around provider calls; keeps model output inside explicit state, execution, and safety boundaries. Owns steering, pursuit, retry, and the autonomous loop. [Harness architecture](../explanation/agent-design/harness.md) |
| **transcript** | The append-mostly message history resent in full on every request — the model's only memory between requests. Never edited to change meaning. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **catalog** (tool catalog) | The list of tool schemas published to the provider on every request; ephemeral to the runtime, republished each round. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **gating stack** | The ordered checks every tool call crosses before running: lookup → write-scope gate → permission broker. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **native tool-call path** | The runtime carries tool calls in its own structured field; nothing executes until the response terminates. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **fallback tool-call path** | For providers without native function calling: the model emits a call as ordinary text, the agent extracts it and promotes it onto the assistant message. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **repeated-call guard** | The only in-loop guardrail: three identical tool calls in a row are stuck, so the fourth is rejected as an error. [Harness architecture](../explanation/agent-design/harness.md) |
| **uncapped agentic loop** | Distinct tool calls and autonomous iterations are uncapped; context compaction is the backstop. [ADR-0009](../adr/0009-uncapped-agentic-loop.md) |
| **hidden user message** | A message that steers the model but is not rendered in the visible transcript (pursuit re-injection, implicit skill body, hook-injected context). [Pursuits](../explanation/agent-design/pursuits.md) |

## Roles

The runtime has one execution engine (`Agent`) that runs in one of two roles.
`agent` is the umbrella term; `principal` and `envoy` name the concrete roles.

| Term | Definition |
|------|------------|
| **agent** | Umbrella term for the execution engine (`Agent`, crate `neenee-agent`) and the engine-level protocol (`AgentRequest` / `AgentResponse` / `AgentEvent` / `AgentOp`). Every running role is an agent; use `principal` or `envoy` when the role matters. [Harness architecture](../explanation/agent-design/harness.md) |
| **principal** | The top-level, human-facing agent a frontend drives. Owns the visible conversation and the user-tunable `[principal]` config table (`hard_stop_rounds`, `loop_review_enabled`). [Configuration](configuration.md) |
| **envoy** | An isolated child agent the principal spawns via the `envoy` tool to serve a bounded sub-question; fresh history, profile-filtered tools, shares only the provider. See the [Envoys](#envoys) section. [Envoys](../explanation/agent-design/envoys.md) |

## Pursuits and scheduling

| Term | Definition |
|------|------------|
| **pursuit** | A durable, per-session objective: an objective string plus a single `is_complete` boolean. No status machine, no token budget, no checklist. Persisted in SQLite keyed by session id. [Pursuits](../explanation/agent-design/pursuits.md) |
| **objective** | The durable condition to pursue — the end-state statement carried by a pursuit. [Pursuits](../explanation/agent-design/pursuits.md) |
| **stop-gate** | What `/pursue <condition>` arms: at the turn-loop exit it re-injects the condition and forces another round instead of returning. [Pursuits](../explanation/agent-design/pursuits.md) |
| **`[NEENEE_PURSUIT_COMPLETE]`** | The plain-text control signal the model emits to signal pursuit completion; always stripped from visible output. The gate gates, the model signals. [Pursuits](../explanation/agent-design/pursuits.md) |
| **`MAX_PURSUIT_ITERATIONS`** | The 50-round safety cap that bounds a pursuit that never signals completion. [Pursuits](../explanation/agent-design/pursuits.md) |
| **`/repeat` cron scheduler** | Orthogonal clock-driven scheduler: schedules a prompt on a five-field cron expression, stores jobs durably, fires a fresh turn per tick, auto-expires after 30 days. [ADR-0015](../adr/0015-pursue-stop-gate-and-repeat-cron.md) |

## Task list

| Term | Definition |
|------|------------|
| **todo list** | The single source of truth for remaining work, shared with `todo`/`todo_update`, shown in the Activity modal, and persisted across restarts. The model populates it directly; there is no longer a plan tool that seeds it. [ADR-0020](../adr/0020-unified-task-list.md) |
| **stop-gate** | The turn-exit forcing function: the `/pursue` stop-gate plus any `Stop` hooks. It is the only gate that can refuse a turn ending and force one more round. [Harness architecture](../explanation/agent-design/harness.md) |

## Envoys

| Term | Definition |
|------|------------|
| **envoy** | An isolated child agent spawned by the `envoy` tool to investigate a sub-question; shares only the provider with the parent, runs with a fresh history and profile-filtered tools. [Envoys](../explanation/agent-design/envoys.md) |
| **profile** | A declarative bundle (name, system-prompt fragment, and a `ToolPolicy`) that scopes an envoy's behavior; bound by reference by dispatch tools. [Envoys](../explanation/agent-design/envoys.md) |
| **`EXPLORE` profile** | Research role: `Read` ceiling, no write grant; pure read tools. [Envoys](../explanation/agent-design/envoys.md) |
| **`REVIEW` profile** | Read-only transcript auditor role used by the session-review diagnostic. [ADR-0016](../adr/0016-session-review-over-round-counting.md) |
| **`TITLE` profile** | Read-only role used to generate a session title in a single model call. [ADR-0022](../adr/0022-session-level-ai-title.md) |
| **full-duplex** | An envoy is not fire-and-forget: requests travel up to the parent, replies travel down to the exact child. [ADR-0029](../adr/0029-full-duplex-subagent-communication.md) |

## Tools and capabilities

| Term | Definition |
|------|------------|
| **`ToolAccess`** | An ordered enum (`Read < Execute < Write`); variant order is load-bearing. Each consumer expresses its rule as a threshold. [Tool access](tools/access.md) |
| **`Read` tier** | Inspects state, no side effects. Admitted by every envoy profile; bypasses the permission broker. [Tool access](tools/access.md) |
| **`Execute` tier** | Runs commands; may have external side effects but is not a file-mutation primitive. Broker-prompted. [Tool access](tools/access.md) |
| **`Write` tier** | The tool's purpose is to mutate the workspace. Broker-prompted unless covered by a `write_paths` grant. Default when a tool does not override `access()`. [Tool access](tools/access.md) |
| **capability axes** | Beyond `access()`, the `Tool` trait exposes `requires_user()` and `spawns_envoy()`, consulted for envoy admission. [Tool access](tools/access.md) |
| **`ToolPolicy`** | An envoy profile's policy: an `access` ceiling, an `allow_user_interaction` flag, and a `write_paths` grant. [Tool access](tools/access.md) |
| **ceiling** | The ordered `ToolAccess` threshold a profile admits tools at or below. [Envoys](../explanation/agent-design/envoys.md) |
| **`write_paths` grant** | A declarative relative-dir spec on `ToolPolicy`; admits a `Write` tool below the ceiling, then scoped at runtime. [ADR-0028](../adr/0028-capability-allocation-scoped-writes.md) |
| **`WriteScope`** | A runtime, per-agent filesystem-write boundary (`None` / `Scoped` / `Unrestricted`); a hard boundary, not a prompt. [ADR-0028](../adr/0028-capability-allocation-scoped-writes.md) |
| **write-scope gate** | The gating-stack step (after lookup, before the broker) that blocks write tools whose target is outside the agent's `WriteScope`. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |
| **permission broker** | The interactive authorization surface: Write/Execute tools pass through it before execution; offers once/always/reject. [Harness architecture](../explanation/agent-design/harness.md) |
| **unattended** | When on, the harness stops prompting for confirmation before write tools. Affects the live process only. [Slash commands](commands.md) |
| **`tool_call_id` pairing** | The wire requirement that every result message references a preceding call id; preserved across pruning and fallback. [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) |

## Skills

| Term | Definition |
|------|------------|
| **skill** | On-demand domain expertise: a Markdown document with a small YAML header whose body is injected into the conversation when needed. Not a tool — carries no executable code. [Skills](../explanation/agent-design/skills.md) |
| **`SKILL.md`** | The skill file inside its own directory (so it can carry auxiliary files); YAML frontmatter declares identity/behavior. [Skills](../explanation/agent-design/skills.md) |
| **catalog channel** | Each enabled skill's name and one-line description, placed in the system prompt every turn at near-zero cost. [Skills](../explanation/agent-design/skills.md) |
| **body channel** | The full Markdown expertise document, delivered on demand only. [Skills](../explanation/agent-design/skills.md) |
| **skill scope** | The ordered source priority cascade (lowest→highest): System, Remote, User, Extra, Repo. Higher scope overrides a same-named lower scope. [Skills](../explanation/agent-design/skills.md) |
| **bundled skills** | Compile-time-embedded into the binary; never on disk, no install step. [ADR-0013](../adr/0013-skills-xdg-paths-and-bundled-embed.md) |
| **implicit invocation** | Mention detection: the harness scans the latest user message for skill mentions and loads allowed skills as a hidden user message. [Skills](../explanation/agent-design/skills.md) |

## Context projection

| Term | Definition |
|------|------------|
| **model context** | The provider-facing request view for one round: rebuilt system prompt, current model window, and current tool catalog serialized for the selected provider. [Model context](../explanation/agent-design/model-context.md) |
| **model-context projection** | The durable archive-and-replace operation that records original context in the session store and produces the model-visible window sent on later provider requests. [Session persistence](../explanation/agent-design/session-persistence.md) |
| **model window** | The current model-visible message window restored on resume and sent to the provider after prompt assembly and provider-specific filtering. [Model context](../explanation/agent-design/model-context.md) |
| **archived transcript** | Original messages moved out of the model window by pruning or compaction but retained in the durable session for full recovery. [Session persistence](../explanation/agent-design/session-persistence.md) |
| **context pruning** | The cheap first projection layer: clears stale tool-result bodies while preserving the `tool_call_id` chain. [Context pruning](../explanation/agent-design/context-pruning.md) |
| **context compaction** | The heavier second projection layer: summarizes older complete turns into a durable checkpoint with a visible `Compacted` notice. [Context compaction](../explanation/agent-design/context-compaction.md) |
| **overflow recovery** | The reactive backstop: if a provider reports context overflow before any tool event, the runner may compact and retry once. [Harness architecture](../explanation/agent-design/harness.md) |
| **pressure** | Context size estimated in tokens (~4 chars/token), compared against thresholds derived from the active model's context window. [Configuration](configuration.md) |

## Providers

| Term | Definition |
|------|------------|
| **provider** | An LLM backend implementing the `Provider` trait; selected at startup and on `/provider` switch. [Providers](providers.md) |
| **`Channel`** | The fully resolved materialization of a provider id: credentials, model id, and transport; one per `[[providers.channels]]` entry. [Providers](providers.md) |
| **transport** | The wire protocol a channel uses (`openai_compat`, `gemini_native`, `llama`). [Configuration](configuration.md) |
| **model catalog** | Centralized provider-construction factory; every provider id materializes into a `Channel`, so startup and runtime switching share one resolution source. [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| **`RetryableError`** | The marker type wrapping transient provider errors; prefixed `[NEENEE_RETRYABLE]`. [Providers](providers.md) |
| **provider retry** | Turn-level retry loop: transient HTTP 408/429/5xx failures retried with bounded exponential backoff; retryable errors become terminal once any tool has run. [Harness architecture](../explanation/agent-design/harness.md) |

## Persistence

| Term | Definition |
|------|------------|
| **durable session** | The local recoverable scene for one coding session: durable transcript, model window, archived transcript, title, task list, pursuit state, and projection metadata. [Session persistence](../explanation/agent-design/session-persistence.md) |
| **admission** | Writes the visible or hidden user message before provider work; each turn records its admission session id. [Harness architecture](../explanation/agent-design/harness.md) |
| **XDG layout** | Files classified by nature and routed to Config, Data, State, Cache, or Runtime categories with different operational lifetimes. [Persistence](../explanation/persistence.md) |
| **override precedence** | Who decides a path, highest→lowest: CLI flag → app env (`NEENEE_*_DIR`) → standard XDG env → native per-OS default → `$HOME` fallback → current directory. [Persistence](../explanation/persistence.md) |
| **per-project bucket** | Under Data; keeps each working directory's history isolated. The hash is short (16 hex chars / 64 bits). [Persistence](../explanation/persistence.md) |
| **advisory lock** | Process-level single-instance-per-project lock; falls back to State when no runtime dir is available. [ADR-0018](../adr/0018-per-project-multi-instance-concurrency.md) |

## Hooks

| Term | Definition |
|------|------------|
| **lifecycle hook** | A user-configured shell command that runs automatically at a specific point in the agent's lifecycle. [Lifecycle hooks](../explanation/agent-design/hooks.md) |
| **lifecycle event** | The events hooks fire on: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `Round`, `PreCompact`, `PostCompact`. [Lifecycle hooks](../explanation/agent-design/hooks.md) |
| **implicit capability** | What a hook may do is implied by its event, not a knob: `PreToolUse`/`Stop` may deny; `PostToolUse`/`UserPromptSubmit`/`PreCompact` may inject context; the rest only observe. [Lifecycle hooks](../explanation/agent-design/hooks.md) |
| **matcher** | A tool-name filter on the tool events: a `|`-separated exact-name list, or a regex; omitted/`*` matches all. [Lifecycle hooks](../explanation/agent-design/hooks.md) |

## Prompts

| Term | Definition |
|------|------------|
| **prompt channel** | One of the two composition targets for harness-assembled text: `System` (the head system message) and `User` (any harness-injected user-role message). [ADR-0039](../adr/0039-unified-prompt-registry.md) |
| **`PromptSection`** | A declarative, self-contained prompt fragment: an id, a channel, an `InjectionKind`, a `rank`, an `is_active` predicate, and a `render`. One section == one injection path == one `InjectionKind` variant. [ADR-0039](../adr/0039-unified-prompt-registry.md) |
| **prompt registry** | The single entry point that collects `PromptSection`s per channel, sorts by `rank`, renders, and stamps `InjectionOrigin` — replacing the per-site `format!`/`push_str`/`Vec::join` assembly. [ADR-0039](../adr/0039-unified-prompt-registry.md) |
| **`PromptContext`** | The read-only view (identity preamble, pursuit, tool names, skills index, last user text) a section's `render` draws from; plain owned data so it lives in core without a reverse edge into `neenee-agent`. [ADR-0039](../adr/0039-unified-prompt-registry.md) |

## Architecture

| Term | Definition |
|------|------------|
| **`neenee-core`** | Pure domain crate: `ToolAccess`, `Provider`/`Tool` traits, `EnvoyProfile`/`ToolPolicy`, `WriteScope`, config-schema types, value types. [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| **`neenee-store`** | The local coding-agent persistence layer: event-sourced session, blob store, config, paths, embedding index, advisory locks, telemetry. [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| **`neenee-providers`** | Provider implementations and the `build_provider_for_channel` factory; a peer of tools/store, depending only on core. [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| **`neenee-tools`** | Built-in domain tools depending only on core; a peer of providers/store. [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| **`neenee-agent`** | The orchestration layer; primary export is the `Agent` struct. Re-exports all of `neenee-core`. [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| **`neenee-code`** | The crate producing the `neenee-code` binary; assembles concrete tool/provider instances and contains the TUI. The *coding* application. [ADR-0035](../adr/0035-application-layer-split.md) |
| **`neenee-quant`** | The *quantitative-trading* application crate, a peer of `neenee-code` at the application layer; depends on `neenee-agent` and provides its own quant domain tools (and a future GUI). Application-layer: core ← {providers, tools, store} ← agent ← {code, quant}. [ADR-0035](../adr/0035-application-layer-split.md) |
| **`Agent`** | The central type in `neenee-agent`; owns the turn/round loop, gates, pursuit cell, permission broker, and `WriteScope`. [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| **strict layering** | The dependency graph is strictly layered with zero reverse edges: core ← {providers, tools, store} ← agent ← {code, quant}. [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| **`QUANT` profile** | A bounded envoy profile admitting read-only quant tools (`market_data`, `backtest`, `list_positions`) plus shared read-only inspection, while excluding live trading (`place_order`) and all coding write/edit/exec tools — domain isolation between the coding and quant applications. [ADR-0035](../adr/0035-application-layer-split.md) |
| **MCP server** | A local stdio MCP server exposing dynamically discovered tools; surfaces as `mcp__<server>__<tool>`. [MCP servers](../explanation/agent-design/mcp.md) |

## Legacy terms

Terms superseded by the decisions above, retained for reading older
documentation and ADRs.

| Term | Superseded by | Reference |
|------|---------------|-----------|
| `neenee-app` | `neenee-store` | [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| `neenee-cli` | `neenee-code` | [ADR-0035](../adr/0035-application-layer-split.md) |
| `neenee` (binary) | `neenee-code` | [ADR-0035](../adr/0035-application-layer-split.md) |
| `neenee-harness` | `neenee-agent` | [ADR-0005](../adr/0005-strict-layering-and-renames.md) |
| `/goal` + `/loop` | `/pursue` + `/repeat` | [ADR-0015](../adr/0015-pursue-stop-gate-and-repeat-cron.md) |
| `[NEENEE_GOAL_COMPLETE]` | `[NEENEE_PURSUIT_COMPLETE]` | [ADR-0015](../adr/0015-pursue-stop-gate-and-repeat-cron.md) |
| Plan mode | plan-as-an-envoy | [ADR-0027](../adr/0027-plan-as-subagent.md) |
| per-plan progress panel | unified todo list | [ADR-0020](../adr/0020-unified-task-list.md) |
| `plan` / `verify_plan_execution` tools | removed (planning is prompt-level) | [ADR-0033](../adr/0033-remove-plan-and-verify-workflow.md) |
| `PLAN` / `VERIFY` profiles | removed | [ADR-0033](../adr/0033-remove-plan-and-verify-workflow.md) |
| verify-nudge / todo-continuation nudge | stop-gate (pursue + `Stop` hooks) | [ADR-0033](../adr/0033-remove-plan-and-verify-workflow.md) |
| stall detector | session-review diagnostic | [ADR-0009](../adr/0009-uncapped-agentic-loop.md) |

## See also

- [Turns and rounds](../explanation/agent-design/turns-and-rounds.md) — the
  two-layer execution model
- [Harness architecture](../explanation/agent-design/harness.md) — the
  control plane
- [ADR-0005](../adr/0005-strict-layering-and-renames.md) — the crate
  topology and naming
