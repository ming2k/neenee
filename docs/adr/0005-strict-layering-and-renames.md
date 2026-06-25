# 0005. Strictly-layered topology + scenario-bound store

- **Status:** Accepted (revised by ADR-0035 for the application-layer rename `neenee-cli` → `neenee-code`; the topology fix and `app`/`harness` renames below still stand)
- **Date:** 2026-06-20
- **Supersedes:** ADR-0004 (which codified the six-crate split but left two
  reverse dependency edges and a misleadingly generic `neenee-app` name)

## Context

ADR-0004 split the workspace into six crates (`neenee-core`,
`neenee-app`, `neenee-providers`, `neenee-tools`, `neenee-harness`,
`neenee-cli`) and documented the intended layering. Two issues remained
after it landed:

1. **Reverse dependency edges broke the layered story.**
   - `neenee-app` depended on `neenee-providers` because `catalog.rs`
     (the model → concrete-`Provider` factory) lived in store but built
     providers. Store sitting above providers felt wrong — store is
     conceptually infrastructure, providers are an implementation peer.
   - `neenee-tools` depended on `neenee-harness` because `TaskTool`
     spawned sub-agents via `Agent::new`. Tools sitting above the
     orchestration layer felt wrong — tools are below the agent.

2. **Two crate names described mechanism, not purpose.**
   - `neenee-app` read as "the application itself", but the crate is
     really the local coding-agent persistence layer (event-sourced
     session, blob store, config, paths, embedding index, advisory
     locks, telemetry).
   - `neenee-harness` required the reader to know what a test/coding
     harness is. The crate's primary export is the `Agent` struct.

A separate concern also surfaced during review: `neenee-app`'s design
assumed a single-user workstation (XDG paths, single-instance flock per
project, file-backed event log). If the project grows other scenarios
(group chat with multi-tenancy, always-on quant trading), this layer
will not fit them and should not be bent to try.

## Decision

### 1. Fix the topology by relocating two misplaced modules

Move the code that created the reverse edges **into** the orchestration
layer, where wiring concrete things together is the whole job:

| Module | From | To | Reason |
|--------|------|----|--------|
| `catalog.rs` (build concrete `Provider` from `Config`) | `neenee-store` | `neenee-agent` | The catalog is a factory consumed by orchestration. It needs `Config` (in store) and concrete provider impls (in providers); the orchestration layer already depends on both, so it is the natural home. |
| `TaskTool` (spawns sub-agents) + supporting `SubAgentOutcome` + `forward_event` + its 2 tests + the `CannedProvider`/`EchoReadTool` test helpers | `neenee-tools` | `neenee-agent` | `TaskTool` is fundamentally an orchestration primitive that happens to satisfy the `Tool` trait. It needs `Agent::new`, so it lives where `Agent` lives. |

`search_tool.rs` stays in `neenee-store` deliberately. It is a
store-feature (semantic search over session history) exposed as a
`Tool`, not an orchestration concern. It does not create any reverse
edges, so the relocation pattern does not apply.

After the move the graph is strictly layered with zero reverse edges:

```
neenee-core        (no workspace deps)
       ^
       │
neenee-providers   ─┐
neenee-tools        │  three peers; none depend on each other
neenee-store       ─┘  (each depends only on core)
       ^
       │
neenee-agent       (core + store + providers)
                    ^ catalog.rs and TaskTool now live here, so agent
                      needs providers + store. Agent does NOT depend on
                      tools at lib time — TaskTool consumes the `Tool`
                      trait from core, not any concrete tool.
       ^
       │
neenee-cli         (everything; assembles concrete tool/provider instances)
```

The skill tools (`UseSkillTool` / `ListSkillsTool` / `ReloadSkillsTool`)
live in `neenee-agent::skills::tools` because skills are an
orchestration concept (loaded by the agent's prompt-assembly path).
The previous convenience re-export from `neenee-tools` is removed; it
would have re-introduced the tools → agent edge.

`neenee-store`'s session tests use `MockProvider` for compaction tests,
so `neenee-providers` is a *dev*-dep of store (not a regular dep).
Dev-deps do not affect the lib's dependency graph, so store remains a
pure peer of providers/tools at build time.

### 2. Rename the two crates whose names described mechanism

| Old | New | Rationale |
|-----|-----|-----------|
| `neenee-app` | **`neenee-store`** | The crate is the local coding-agent persistence layer. `*-store` is industry convention (servo, tikv, many ORMs). "app" misleads readers into thinking it is the product itself. |
| `neenee-harness` | **`neenee-agent`** | The crate's primary export is the `Agent` struct. Naming a crate after its central type is Rust convention (tokio, serde, reqwest). "harness" is jargon. |

The binary stays `neenee` (via `[[bin]] name = "neenee"` in
`neenee-cli`). The crate that produces it is `neenee-cli` so the
workspace reads symmetrically against a future `neenee-gui`.

### 3. Document the scenario scope of `neenee-store`

`neenee-store/src/lib.rs` now states explicitly that this is the
**local coding-agent** persistence layer:

- Paths resolve via XDG `ProjectDirs` (assumes single-user filesystem).
- Sessions are keyed by project root (one active session per project).
- A process-level `flock` enforces single-instance-per-project.
- State is file-backed (JSON Lines event log + content-addressed blobs).

Other scenarios the project may grow — multi-tenant group chat,
always-on autonomous quant trading — will not fit these assumptions.
They should spawn **sibling crates** (`neenee-chat-store`,
`neenee-trading-store`) that share only `neenee-core` (and maybe
`neenee-providers`), not bend this crate to fit. Rule of three: when
three scenarios share a concept (e.g. goal tracking, context
compaction), *then* extract that concept into a trait in core.

### 4. What stays the same from ADR-0004

- The six crates (now under their new names).
- `neenee-core` is pure domain (zero I/O).
- `Channel::build` is the free function `build_provider_for_channel` in
  `neenee-providers`.
- Config-schema types (`WebSearchConfig`, `McpServerConfig`,
  `McpConnectionStatus`, `SkillsConfig`) live in core because both
  store's `Config` and the implementation crates need them.
- `neenee-agent` re-exports all of `neenee-core` (with an explicit list
  because Rust's glob imports do not propagate through re-exports).
- The TUI is inlined into the binary as `mod tui`.

## Alternatives considered

- **Define `SessionStore` / `Config` traits in core, have store provide
  one impl.** Tempting for future scenarios but premature: there is
  only one consumer today. Defining the trait now means guessing the
  boundary. Wait for a real second consumer (chat/trading), then
  extract with two concrete data points. Doc the contract in
  `neenee-store` so the future extraction is mechanical.

- **Prefix scenario-specific crates now (`neenee-coding-tools`,
  `neenee-coding-store`, …).** Rejected: only one scenario exists, so
  the prefix adds verbosity without disambiguation. When the second
  scenario arrives, `git mv` is cheap.

- **Keep `neenee-app` name, accept the ambiguity.** Rejected: names
  shape how new contributors read the codebase. Six months from now
  "what does neenee-app contain?" will be a recurring question. Six
  months from now "what does neenee-store contain?" answers itself.

- **Inline `TaskTool` into the binary rather than into
  `neenee-agent`.** Rejected: a GUI would also want sub-agent spawning,
  so keeping `TaskTool` below the binary (in agent) preserves
  reusability. Putting it in the binary would force the GUI to
  reimplement it.

## Consequences

- **Positive.** The dependency graph is now a strict DAG with no
  reverse edges. `cargo tree -d` shows no surprises. Build
  parallelism improves further: touching `catalog.rs` no longer
  recompiles store's `session.rs`.

- **Positive.** The two renamed crates are self-describing. New
  contributors can navigate the workspace without a glossary.

- **Positive.** The scenario scope of `neenee-store` is now explicit.
  When group-chat or quant-trading work begins, the right move
  (sibling crate, not bending this one) is documented and obvious.

- **Neutral.** The binary's `Cargo.toml` lists six path dependencies
  instead of five; one more line of `[dependencies]`.

- **Neutral.** `neenee-store` carries one `Tool` implementation
  (`SearchHistoryTool`). It is a leak in the strict "store has no
  tools" story, but it does not create any reverse dependency edges
  and it accurately reflects "this tool is a query interface over
  store-owned state". Documented as a deliberate exception.

## Migration mechanics

Two commits, in dependency order:

| Commit | What | Files touched |
|--------|------|---------------|
| `ee9fca1` topology fix | `catalog.rs` moved store → agent; `TaskTool` + tests moved tools → agent; store loses providers dep; tools loses agent dep; `neenee-tools::UseSkillTool` re-export dropped, callers reach into `neenee-agent::skills::tools` directly | 10 |
| `96850eb` renames | `crates/neenee-app` → `crates/neenee-store`; `crates/neenee-harness` → `crates/neenee-agent`; package names + path deps + every `use neenee_app::`/`use neenee_harness::` updated; doc comments refreshed | 35 |

Both commits build clean and pass all 355 tests.

## References

- ADR-0001 — tool-step rendering (unchanged).
- ADR-0002 — model/channel abstraction; `Channel::build` is still
  `build_provider_for_channel` in `neenee-providers`, now consumed by
  the catalog that lives in `neenee-agent`.
- ADR-0003 — original four-crate split (superseded by ADR-0004).
- ADR-0004 — six-crate topology; superseded by this ADR for the
  topology fix and renames. The TuiConfig data/policy split and
  the inlined-TUI decision from ADR-0004 still stand.
