# 0004. Six-crate topology: core / app / providers / tools / harness / cli

- **Status:** Superseded by ADR-0005
- **Date:** 2026-06-20
- **Supersedes:** ADR-0003 (which framed the topology as four crates with a
  standalone `neenee-tui`; that boundary was correct at the time but the
  GUI decision prompted a deeper split and the inline of the TUI)

## Context

ADR-0003 introduced `neenee-app` and reorganised the workspace into four
crates — `neenee-core`, `neenee-app`, `neenee-tui`, `neenee` (binary). That
fix moved eleven application-service modules out of the binary but left two
structural problems:

1. **`neenee-core` was not pure.** It contained 1500 lines of HTTP
   `Provider` implementations (`providers.rs`), 1600 lines of `Tool`
   implementations (`tools.rs` + `mcp.rs` + `commands.rs` + `project.rs`),
   the stateful `Agent` struct with its turn loop (`agent.rs`), and the
   skills subsystem (filesystem discovery, remote fetch, in-process
   registry). Anything depending on `neenee-core` — including the future
   GUI — pulled in `reqwest`, `rusqlite`, `walkdir`, and every concrete
   provider and tool.
2. **The orchestration policy was trapped in the binary.** `main.rs`
   carried ~600 lines of `execute_turn`, compaction gates, retry policy,
   and the autonomous goal loop. The slash-command dispatch that drives
   them is genuinely CLI-specific (text commands, `/loop`, `/goal`), but
   the turn machinery itself is identical for any frontend. A GUI would
   have had to either fork this code or depend on the binary.

A GUI built on `../flux` (immediate-mode UI via `flux-ui` Rust bindings)
was approved as a near-term goal, which made the rule-of-three argument
against pre-extraction no longer apply: the second consumer was real, not
hypothetical.

## Decision

Split the workspace into six crates with a strict acyclic dependency graph:

```text
neenee-core        Pure domain vocabulary (types & traits only)
    ^
neenee-providers   impl Provider (HTTP: OpenAI/Gemini/DeepSeek/Qwen/GLM/Kimi/Llama/Mock)
neenee-tools       impl Tool (Bash/Read/Write/Edit/Glob/Grep/Web… + MCP loader)
    ^
neenee-app         Persistence, config, paths, catalog (unchanged from ADR-0003)
    ^
neenee-harness     Agent struct + turn orchestration (execute_turn, compaction,
                    retry, goal accounting, autonomous loop, ProxyProvider)
    ^
neenee-cli         Binary: main + slash-command dispatch + TUI rendering
                    (merges the former neenee-tui inline as `mod tui`)
```

The future `neenee-gui` is a sibling binary that depends on
`neenee-harness` + `neenee-app` + `flux-ui` and replaces slash dispatch
with menu/dialog dispatch and ratatui with the flux canvas.

### Six structural rules that fall out of the split

1. **`neenee-core` has zero I/O.** No `reqwest`, no `rusqlite`, no
   `walkdir`. Provider implementations, tool implementations, and skill
   discovery all live above core. The shared config schemas
   (`WebSearchConfig`, `McpServerConfig`, `McpConnectionStatus`,
   `SkillsConfig`) DO live in core because both the app's `Config` and
   the implementation crates need them — the alternative is forcing
   `neenee-app` to depend on `neenee-tools`/`neenee-harness` for plain
   data types, which inverts the dependency direction.

2. **`Channel::build` is a free function in `neenee-providers`.** Core
   defines `Channel` as pure data; the construction path that knows about
   concrete `Provider` impls lives with those impls. Callers use
   `neenee_providers::build_provider_for_channel(channel, entry_id)`.

3. **The Agent struct lives in `neenee-harness`, not core.** It is
   stateful (holds tools, provider, mode, goal, skill registry) and its
   turn loop threads session/config pressure from `neenee-app` — neither
   belongs in pure core. `TaskTool` (in `neenee-tools`) constructs
   sub-agents, so `neenee-tools` depends on `neenee-harness`; the
   relationship is one-way, no cycle.

4. **`neenee-harness` does not depend on `neenee-tools`** at lib time.
   It speaks to tools through the `Tool` trait from core. Concrete tool
   instances are assembled by the binary, which depends on everything.
   `neenee-tools` is a *dev*-dependency of harness so the
   `ask_user_tool_blocks_and_returns_selected_answers` integration test
   can construct a real `AskUserTool`; dev-deps do not form cycles.

5. **`neenee-harness` re-exports all of `neenee-core`.** Rust's glob
   imports do not propagate through re-exports, so `pub use
   neenee_core::*;` is augmented with an explicit re-export list of every
   top-level item `Agent` consumes (`Goal`, `Message`, `Provider`,
   `ProviderStreamEvent`, …). Consumers can `use neenee_harness::*` and
   get the full domain vocabulary alongside `Agent`.

6. **`neenee-tui` is merged into `neenee-cli` as `mod tui`.** With the
   application/harness/providers/tools layers all extracted, the TUI had
   no consumers except the CLI binary. Keeping it as a sibling crate
   added a needless `start_tui` indirection and a `pub` surface driven by
   external-API visibility rules. Inlining lets the binary reach TUI
   internals directly. The crate is renamed `neenee` → `neenee-cli` with
   `[[bin]] name = "neenee"`, so the user-facing command stays `neenee`
   (cargo/git/nvim convention: primary tool gets the bare name,
   alternatives get suffixed → `neenee-gui`).

### Things explicitly NOT done

- **No `Harness` struct wrapping `execute_turn`.** The orchestration
  functions stay as free functions taking a `TurnContext`/`LoopRunContext`
  by value. Wrapping them was tempting for tidiness, but the context
  structs already bundle every dependency and the call sites in main.rs
  are clear. A `Harness` facade can be added later if a second frontend
  finds the boundary awkward.
- **No further splitting of `neenee-harness`.** `agent.rs`, `prompt.rs`,
  `skills/`, `orchestration.rs`, and `tests.rs` all live in one crate.
  `orchestration.rs` is ~850 lines and could plausibly be split into
  `compaction.rs` + `loop.rs` + `provider_proxy.rs`, but doing so now
  would pre-empt a real need. Defer until a concrete pain point appears.
- **No extraction of `neenee-tools`'s skill helpers.** `SkillRegistry`
  moved with `Agent` to `neenee-harness` because the agent struct owns
  one. The skill tool implementations (`UseSkillTool`, `ListSkillsTool`,
  `ReloadSkillsTool`) are re-exported from `neenee-tools` so the binary's
  import shape is unchanged.

## Alternatives considered

- **Keep `neenee-tui` as a sibling crate.** Rejected: no remaining
  consumer, and it would have made `neenee-gui` look like a peer to a
  frontend that was already inlined. The merge also lets the binary's
  `pub` dead-code warnings be silenced with a single `#[allow(dead_code)]
  mod tui;` rather than spreading `#[allow(dead_code)]` across many items
  that used to be crate-public.

- **Move the Agent struct into `neenee-core` (and skills back too).**
  Rejected: the Agent struct is stateful, depends on `SkillRegistry`
  which does I/O, and its turn loop threads `SessionStore`/`Config`
  pressure. All three pull `neenee-app` (or worse) into core, breaking
  the "core is pure types" rule that motivated the whole refactor.

- **Extract `neenee-providers` only, leave tools in core.** Rejected for
  symmetry: tool impls are the dual of provider impls (both are concrete
  implementations of a core trait), so extracting one and not the other
  leaves an inconsistent core. Tools also bring their own I/O (`reqwest`
  for web tools, `tokio::process` for bash, filesystem walks) which
  belongs above core, not in it.

- **Rename the binary to `neenee-cli` as the user-facing command.**
  Rejected: `neenee` is shorter, matches the convention used by
  cargo/git/nvim (primary tool is bare-named, alternatives suffixed), and
  preserves every existing user invocation. The crate name still encodes
  the CLI-vs-GUI distinction for workspace clarity.

## Consequences

- **Positive.** A `neenee-gui` crate can land as a sibling binary with
  deps `{neenee-core, neenee-app, neenee-harness, flux-ui, flux-ui-shell}`
  and zero refactor of existing crates. It speaks the same
  `AgentRequest`/`AgentResponse` mpsc protocol the TUI uses, drives the
  same `execute_turn`, and substitutes only the rendering layer (flux
  canvas for ratatui) and the input layer (menus/dialogs for slash
  commands).
- **Positive.** Build parallelism improves — touching `providers.rs` no
  longer recompiles `tools.rs`, touching `agent.rs` no longer recompiles
  providers/tools. Test granularity likewise improves: each crate's
  tests run independently.
- **Positive.** `neenee-core` is genuinely pure. Auditing the domain
  vocabulary, or reusing it in a different harness, no longer drags in
  HTTP and SQLite.
- **Neutral.** Six crates instead of four. The Cargo boilerplate cost is
  real but small; each manifest is focused.
- **Negative (mild).** `neenee-harness` has an explicit re-export list of
  ~30 names that must be kept in sync with `neenee-core`'s lib.rs. This
  is a Rust limitation (glob imports don't propagate through re-exports)
  rather than a design choice; a comment in harness's lib.rs flags it.
- **Negative (mild).** One test (`ask_user_tool_blocks_and_returns_selected_answers`)
  was temporarily dropped during the tools extraction (commit `25f4d43`)
  to avoid a cyclic dev-dep, and restored during the harness extraction
  (commit `2a26539`) once `tests.rs` lived in harness and the tools crate
  became a normal dev-dep. The intermediate state lost 1 of 355 tests
  for one commit; the final state runs all 355.

## Migration mechanics

Four commits, in dependency order (each one builds and tests clean):

| Commit | Moves | Files touched |
|--------|-------|---------------|
| `e5ef917` providers | `providers.rs` → `neenee-providers/src/lib.rs`; `Channel::build` → free fn `build_provider_for_channel` | 12 |
| `25f4d43` tools | `tools.rs` + `tools/search/` + `mcp.rs` + `commands.rs` + `project.rs` → `neenee-tools/`; `WebSearchConfig`/`McpServerConfig`/`McpConnectionStatus` relocate to core | 21 |
| `2a26539` harness | `agent.rs` + `prompt.rs` + `tests.rs` + `skills/` → `neenee-harness/`; orchestration extracted from `main.rs` into `orchestration.rs`; `SkillsConfig` relocates to core | 22 |
| `c528cea` cli merge | `neenee-tui/src/*` → `neenee-cli/src/tui/*`; crate renamed `neenee` → `neenee-cli` with `[[bin]] name = "neenee"` | 48 |

Total: 355/355 tests pass at HEAD. Binary at `target/release/neenee` is
unchanged from the user's perspective.

## References

- ADR-0001 — tool-step rendering redesign (the `[tui.default_expanded]`
  table and presenter lookup that motivated splitting `TuiConfig` into
  data + policy layers in ADR-0003; that split survives this refactor
  intact).
- ADR-0002 — model/channel abstraction (`Channel::build`, now
  `build_provider_for_channel` in `neenee-providers`).
- ADR-0003 — original four-crate split (`neenee-core`/`neenee-app`/
  `neenee-tui`/`neenee`); this ADR supersedes the topology section while
  keeping the `TuiConfig` data/policy split.
- `../flux/ui/README.md` — the immediate-mode UI library the future
  `neenee-gui` crate will consume via `flux-ui` Rust bindings.
