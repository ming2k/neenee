# 0035. Application-layer split: `neenee-code` + `neenee-quant`

- **Status:** Accepted
- **Date:** 2026-06-26
- **Revises:** ADR-0005 (the "binary stays `neenee` / crate is `neenee-cli`"
  sub-decision); instantiates the application-layer sibling ADR-0005
  anticipated)

## Context

ADR-0005 established the strictly-layered topology, renamed `neenee-app` →
`neenee-store` and `neenee-harness` → `neenee-agent`, and closed with a
sub-decision:

> The binary stays `neenee` (via `[[bin]] name = "neenee"` in `neenee-cli`).
> The crate that produces it is `neenee-cli` so the workspace reads
> symmetrically against a future `neenee-gui`.

That decision assumed a single application binary — the coding tool — was the
product's primary surface, and that a GUI would be its only sibling. Two forces
made that assumption no longer hold:

1. **`neenee-cli` describes mechanism, not domain.** ADR-0005 renamed
   `app`/`harness` precisely because crate names should describe *purpose*.
   `*-cli` only says "this is the command-line frontend"; it says nothing about
   what the application *does*. 0005's own rationale ("harness is jargon";
   "app misleads readers") applies equally to "cli".

2. **A second application now exists: quantitative trading.** 0005's
   "scenario scope" section explicitly anticipated sibling application crates
   for new scenarios (always-on quant trading was named directly). That
   scenario has arrived, so the bare `neenee` name — which claimed to be *the*
   product binary — no longer disambiguates. With two domain applications there
   is no single primary; each should carry its domain.

## Decision

### 1. Rename the coding application: crate `neenee-cli` → `neenee-code`, binary `neenee` → `neenee-code`

| Old | New | Rationale |
|-----|-----|-----------|
| `neenee-cli` (crate) | **`neenee-code`** | Names the domain (coding), symmetric with the new `neenee-quant`. `*-code`/`*-quant` describe what the application *is*; `*-cli` described only the transport. |
| `neenee` (binary) | **`neenee-code`** | `[[bin]] name = "neenee-code"` in `neenee-code/Cargo.toml`. With two domain commands, neither is bare-named "primary"; the domain names the command. Matches the `neenee-quant` sibling. |

### 2. Add `neenee-quant` as an application-layer peer of `neenee-code`

The application layer now has two sinks, both depending on `neenee-agent` only:

```
neenee-core        (no workspace deps)
       ^
       │
neenee-providers  ─┐
neenee-tools       │  three peers; none depend on each other
neenee-store      ─┘  (each depends only on core)
       ^
       │
neenee-agent       (core + store + providers; the Agent + orchestration)
       ^
       │
neenee-code  ─┐    application layer: each assembles its OWN tool/provider
neenee-quant ─┘    set and owns its frontend. Both depend on agent only,
                   never on each other.
```

The strict-DAG property from ADR-0005 is preserved: the application layer adds
two sinks and zero reverse edges.

### 3. Quant tools do NOT self-register — domain isolation at assembly time

The coding tools in `neenee-tools` self-register via
[`register_tool!`](../../crates/neenee-core/src/tool_registry.rs) so the coding
binary collects them with one `collect_tools` call. The quant tools
(`market_data`, `backtest`, `place_order`, `list_positions`) deliberately do
**not**:

- A coding agent must never see a `place_order` tool in its schema list.
- A quant agent must never see `write_file` / `edit_file`.
- Mixing the two registries would bloat context and invite wrong-domain calls.

Each quant tool is therefore a plain struct with a constructor, and the quant
application instantiates exactly the set it wants before handing them to
`Agent::new`. Tool/role isolation is enforced at **assembly time**, not by
runtime filtering. The matching admission policy for bounded quant sub-agents
is the [`QUANT`](../../crates/neenee-core/src/subagent.rs) profile (read-only
quant tools + shared read-only inspection; excludes `place_order`, coding
write/edit, and `bash`).

### 4. What stays the same from ADR-0005

- The `core ← {providers, tools, store} ← agent` layering and the two module
  relocations (`catalog.rs` → agent, `TaskTool` → agent).
- The strict-DAG, zero-reverse-edge property.
- `neenee-store`'s local coding-agent scenario scope.
- `neenee-agent` re-exporting all of `neenee-core`.
- The TUI inlined into the coding binary as `mod tui`.

## Alternatives considered

- **Keep the binary `neenee`, add only `neenee-quant` as a new command.**
  Rejected: a bare `neenee` would *implicitly* mean "the coding one", an
  invisible coupling between a generic name and one domain. Two domain
  commands should both carry their domain so neither is privileged by
  ambiguity.

- **Put the quant tools in `neenee-tools` behind a cargo feature.** Rejected:
  `neenee-tools` is, by ADR-0005, the *coding* toolset. Feature flags are a
  build-time hack for what is an assembly-time concern, and a coding agent
  built without the feature off could still risk surfacing `place_order`.

- **Self-register quant tools and filter them out at runtime.** Rejected:
  runtime filtering is brittle and inverts the contract. The requirement
  ("tools 分配应该不同, 不要搞混") is enforced most reliably by never mixing
  the registries in the first place.

- **Make `neenee-quant` a subcrate/child of `neenee-code`.** Rejected: they
  share only `neenee-agent`. A parent/child relationship would invent a
  dependency edge that does not exist in the DAG and would re-introduce the
  "one binary owns everything" shape this ADR removes.

## Consequences

- **Positive.** Each application is self-describing: the crate and binary name
  state the domain. A new contributor reads `neenee-code` / `neenee-quant` and
  knows what each does without a glossary.

- **Positive.** Domain isolation is structural, not conventional. A coding
  agent literally cannot link `place_order`; a quant agent literally cannot
  link `write_file`. The mistake is impossible at the type-system/link level.

- **Positive.** The strict DAG is preserved — `cargo tree` stays clean, build
  parallelism is unaffected, and a third application (e.g. `neenee-chat`) lands
  as another sibling sink with no refactor.

- **Negative (mild).** Breaking binary rename: every existing `neenee`
  invocation becomes `neenee-code`. Recorded in `CHANGELOG.md` under
  `[Unreleased]`. `git mv` carried the source tree wholesale, so no code
  references to the old crate name remain.

- **Neutral.** The workspace grows from six crates to seven; the application
  `Cargo.toml` for `neenee-code` is unchanged in shape (six path deps), and
  `neenee-quant` adds `neenee-core` + `neenee-agent` (+ a `neenee-tools`
  dev-dep for its isolation test).

## Migration mechanics

| Commit | What | Files touched |
|--------|------|---------------|
| rename | `crates/neenee-cli/` → `crates/neenee-code/`; package name + `[[bin]] name` → `neenee-code`; workspace `members`, path deps, every `use neenee_cli` / `-p neenee-cli`, snapshot module paths, and doc references updated | 157 |
| quant crate | new `crates/neenee-quant/` (`lib.rs`, `market_data.rs`, `backtest.rs`, `orders.rs`, `Cargo.toml`, isolation test); `QUANT` profile + `QUANT_ANALYSIS_TOOLS` in `neenee-core/src/subagent.rs`; workspace `members` updated | new |

## References

- ADR-0005 — strictly-layered topology + renames. This ADR revises 0005's
  "binary stays `neenee`" sub-decision and instantiates the application-layer
  sibling 0005 anticipated, **without superseding** the topology fix or the
  `app`/`harness` renames, which all still stand.
- ADR-0011 — subagent profiles: capability-axis tool admission. The `QUANT`
  profile extends that axis to the quant domain.
- `crates/neenee-core/src/subagent.rs` — the `QUANT` profile and
  `QUANT_ANALYSIS_TOOLS` allow-list.
- `crates/neenee-quant/src/lib.rs` — the rationale for quant tools staying out
  of the global self-registry.
