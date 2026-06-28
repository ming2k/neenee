# 0041. Tool capabilities and variants: scope (agent) vs override (model)

- **Status:** Accepted
- **Date:** 2026-06-28

## Context

A tool used to be one concrete type — `ReadTextTool` had one `description()`,
one `parameters()`, one `call()`. The only per-model lever was **string
patching**: a `[tool_overrides."<model-id>"]` table in `config.toml` deserialized
to a `ToolOverrides` map, applied at schema-build time in
`Provider::prepare_tools_with` via `Tool::to_openai_function_with`, which swapped
the description text and deep-merged per-parameter JSON-Schema fragments. That
mechanism could only *reword the one implementation*; it could not give a model a
genuinely different tool (different schema shape, behaviour, or contract), and the
ad-hoc patching at the provider boundary was fragile.

Two distinct concerns were also tangled. When we generalized overrides into
per-tool *variants*, an early cut let a `SubagentProfile` pin a variant
(`tool_variants` on the profile, `EXPLORE → read_text="terse"`). That conflated
two things that must stay orthogonal:

- **what set of tools** an agent may use, and
- **which implementation** of each tool it sees.

It also blurred the agent model. A subagent (spawned by the `task` tool, ADR-0011)
is **not a different kind of thing** from the top-level agent — both are `Agent`.
They differ only in *who manages them*: a human manages the top-level agent; the
top-level agent manages its subagents. A subagent therefore cannot be managed by
the human *directly* — its permission prompts and `ask_user` questions route **up**
to the parent and the human's replies route **down** (full-duplex, ADR-0029).

Relevant code: `crates/neenee-core/src/capability.rs` (`Tool`, `Provider`),
`crates/neenee-core/src/tool_registry.rs` (registration), and
`crates/neenee-core/src/subagent.rs` (`SubagentProfile` / `ToolPolicy`).

## Decision

Model tool use along **two orthogonal axes**, and make the agent/subagent/human
management relationship explicit.

### 1. A tool is a capability with variants

- A **capability** is the stable logical tool identity, equal to `Tool::name()`.
  It is the only name the model sees and the key dispatch uses, so history,
  permissions, and rendering are stable across variants.
- A **variant** is one concrete realization of a capability — its own
  `description`, `parameters`, and `call` — identified by `Tool::variant()`
  (default `"default"`). Variants of one capability share `name()` and differ in
  `variant()`. The variant id never reaches the model.
- `collect_toolset()` groups self-registered tools into a `ToolSet` of
  `Capability` entries (`tool_registry.rs`). `ToolSet::resolve(&VariantSelection)`
  yields exactly one variant per capability. Each variant's own
  `to_openai_function()` is authoritative — there is no schema patching at the
  provider boundary.

### 2. Scope is the agent's axis; override is the model's axis

- **Scope (agent / profile)** — *which* capabilities an agent may use. Owned by
  `SubagentProfile::tool_policy` (`ToolPolicy::admits` / `select_tools`). The
  top-level agent admits everything; a profile narrows it. Adding a side-effecting
  tool never silently widens a subagent.
- **Override (model)** — *which variant* of each capability. Owned by
  `VariantSelection`, configured per model under `[tool_variants."<model-id>"]`
  and seeded onto the agent via `Agent::set_variant_selection` (re-seeded on model
  switch by `reseed_tool_variants`). The agent holds a resolved view
  (`resolved_tools`) that both advertising (`prepare_tools`) and dispatch read, so
  switching the selection swaps both the schema *and* the executed implementation.

A `SubagentProfile` carries **no** variant information. Profiles are scope only.

### 3. Both main and sub are agents; the subagent inherits the model's override

A subagent runs the **same** provider/model as its parent, so it inherits the
parent's `VariantSelection`. The subagent's toolset is computed as
**scope ∘ override**: resolve every capability to the model's chosen variant, then
narrow to the profile's scope. Concretely the `SubagentTool` holds a shared handle
to the parent agent's variant selection (`Agent::variant_selection_handle()`,
bound once via `SubagentTool::bind_variant_selection`), snapshots it at spawn, and
runs `profile.select_tools(toolset.resolve(selection))`. The profile decides the
set; the model decides the implementation.

### 4. Management relationship

`human → top-level agent → subagent`. The human manages the top-level agent. The
top-level agent manages its subagents (it hands down scope and mediates I/O). A
subagent is never managed by the human directly: its `PermissionRequest` /
`ask_user` events surface up to the parent harness, which presents them to the
human, and replies route back down through the `SubagentRegistry` handle keyed by
the parent tool-call id (ADR-0029).

## Alternatives considered

- **Keep string overrides (`[tool_overrides]`).** Rejected: can only reword one
  implementation, patches schemas at the provider boundary, and cannot express a
  genuinely different tool. Removed entirely.
- **Put variant selection on the subagent profile.** Rejected: conflates scope
  (agent's axis) with override (model's axis). A subagent is an agent on a model,
  so its variants must come from the model, uniformly with the top-level agent.
- **Let subagents always use default variants.** Rejected: a subagent on a model
  configured for a variant would silently diverge from the parent, violating
  "the model owns override" uniformly.

## Consequences

- **Positive.** The two axes are independent and named: a model can be handed a
  genuinely different implementation of a tool without forking scope, and a
  profile can scope tools without knowing anything about implementations.
  Subagents inherit model overrides automatically. The provider boundary is
  simpler (`prepare_tools` only; no `prepare_tools_with` / `to_openai_function_with`).
- **Negative.** More machinery in core: `ToolSet`/`Capability`, an agent-held
  resolved view behind a lock, and a shared variant-selection handle plumbed into
  the subagent dispatch tool.
- **Migration.** Config key `[tool_overrides."<model>"]` (description/param
  patches) is replaced by `[tool_variants."<model>"]` (`capability = "variant"`).
  Existing single-implementation tools are automatically the `"default"` variant —
  no tool body changes. Authoring an alternate variant is a one-struct +
  one-`register_tool!` addition (see `ReadTextTerseTool` in
  `crates/neenee-tools/src/read.rs`).

## References

- ADR-0011 — sub-agent profiles (capability-axis tool admission; the scope axis).
- ADR-0028 — scoped filesystem writes (`write_paths` on `ToolPolicy`; a scope
  dimension).
- ADR-0029 — full-duplex subagent communication (up/down routing; why the human
  does not manage a subagent directly).
- Code: `crates/neenee-core/src/{capability.rs,tool_registry.rs,subagent.rs}`,
  `crates/neenee-agent/src/{agent.rs,subagent_tool.rs}`,
  `crates/neenee-store/src/config.rs`.
