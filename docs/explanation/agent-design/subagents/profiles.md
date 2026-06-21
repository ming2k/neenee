# Sub-agent profiles

A sub-agent's behaviour is not hardcoded in the dispatch tool. It is the output
of a declarative **profile** — a name, a system-prompt fragment that frames the
role, and a [`ToolPolicy`](admission.md) that scopes what it may touch.
Profiles live in `crates/neenee-core/src/subagent.rs` (domain vocabulary), and
the dispatch tools in `neenee-agent` bind them by reference.

## The two built-in profiles

| Profile | Bound by | Access ceiling | Gets |
|---------|----------|----------------|------|
| `EXPLORE` | `task` | `Read` | Pure read tools (`read_file`, `grep`, `glob`, `list_dir`, …) |
| `VERIFY` | `verify_plan_execution` | `Execute` | Read tools **plus `bash`** for tests/builds/type-checks |

Both are non-interactive (`allow_user_interaction: false`) and non-recursive
(recursion is excluded absolutely, not per-profile — see
[Admission](admission.md)). The profile is the single source of truth;
`TaskTool::new` takes the profile explicitly, and the verifier path goes
through the same `TaskTool`.

## Why two roles instead of one

`EXPLORE` is the research role: pure inspection, no side effects. A researcher
should not run commands — an exploration sub-agent with `bash` could mutate
the workspace or run arbitrary commands, which is wrong for "go find things
and report back".

`VERIFY` is the independent-auditor role. An auditor's most valuable evidence
is *behaviour*: does `cargo test` pass, does it build, does it type-check?
Static inspection alone cannot answer those. So the verifier needs command
execution. But it must still not edit the implementation it is auditing — an
independent auditor that can rewrite the thing it is checking is not
independent.

The two needs (no commands vs. commands-but-no-file-writes) cannot be expressed
by a single Read/Write ceiling. Resolving that is what the
`Read < Execute < Write` tier split is for: `VERIFY`'s `Execute` ceiling admits
`bash` while still excluding `write_file`/`edit_file` (`Write`). See
[ADR-0012](../../../adr/0012-toolaccess-tier-split.md).

## Extending

Adding a third role is a new `const SubagentProfile` plus a binding at the
dispatch site — no orchestration surgery, no changes to `ToolPolicy::admits`.
A future write-capable "executor" role, or an interactive role (one where
`UserQuestionRequest` is genuinely forwarded to the user), would land here. The
profile primitive was introduced in
[ADR-0011](../../../adr/0011-subagent-profiles.md) and extended to two roles +
the tier split in [ADR-0012](../../../adr/0012-toolaccess-tier-split.md).

## See also

- [Admission](admission.md) — how a profile's `ToolPolicy` decides per-tool
  admission.
- [`task`](../../../reference/tools/task.md) and
  [`verify_plan_execution`](../../../reference/tools/plan.md#verify_plan_execution)
  — the two dispatch tools that bind these profiles.
