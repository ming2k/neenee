# Plan verification

`verify_plan_execution` is the second sub-agent scenario — the mechanism behind
the Build-mode prompt's "spawn a verifier before declaring completion"
instruction. It is documented as a tool in
[`verify_plan_execution`](../../../reference/tools/plan.md#verify_plan_execution);
this page covers *why* it is a distinct sub-agent role.

## A second role, not a second `task`

The verifier constructs its own `TaskTool` (so it reuses all the sub-agent
plumbing — isolation, snapshot, event forwarding, failure handling) but binds
the [`VERIFY`](profiles.md) profile instead of `EXPLORE`. The difference is one
axis: `VERIFY`'s access ceiling is `Execute`, so the verifier additionally gets
`bash` to run tests, builds, and type-checks as concrete evidence — while still
excluding file writes, user questions, and recursion.

This is the scenario that forced the `Read < Execute < Write` tier split. An
independent auditor's most useful signal is behaviour — does it compile, do the
tests pass — not just "the code looks right". Static-only verification (what
`EXPLORE` gives) cannot produce that signal. But handing the verifier a
`Write`-ceiling profile would let it edit the implementation it is auditing,
which defeats independence. `Execute` is the tier between them: command
execution without file-write capability. See
[ADR-0012](../../../adr/0012-toolaccess-tier-split.md).

## Clean role/task separation

The verifier's *role* contract — independent, unbiased, may run commands, must
not edit, non-interactive — lives in the `VERIFY` profile's system prompt. The
*task* — which plan to read, the PASS/PARTIAL/FAIL report format, the final
verdict line — is carried in the call's user prompt. Adding a new kind of
verification (a different report shape, a focused scope) is a different user
prompt against the same profile, not a new sub-agent.

## Non-streaming by design

Unlike `task`, the verifier uses the non-streaming call path, so its nested
step does not stream live tokens. A verifier reports a final verdict rather
than an investigation to watch; streaming its token-by-token reasoning would
add noise without adding signal.

## See also

- [Profiles](profiles.md) — `EXPLORE` vs `VERIFY`.
- [Plan mode](../plan-mode.md) — when verification runs (Build mode, after
  `plan_exit`).
- [`verify_plan_execution`](../../../reference/tools/plan.md#verify_plan_execution)
  — parameter reference.
