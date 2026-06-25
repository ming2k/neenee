# `abort`

`AbortTool` (`crates/neenee-tools/src/abort.rs`) is the model's
self-initiated **emergency escape hatch**. When the model detects a stuck state
it cannot recover from — it is repeating the same tool call with identical
arguments (a loop), it is about to perform a dangerous or irreversible
operation, or it has reached a dead end — calling `abort` stops the current
operation and exits neenee gracefully.

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `reason` | string | yes | — | Why the model is aborting (e.g. "stuck repeating webfetch"). Recorded for the user. |

## What happens on abort

Calling `abort` sends an `AgentRequest::Abort` to the harness, which:

1. Cancels the in-flight turn — the same `CancellationToken` path as the user
   pressing `Esc` or `Ctrl+C`. The turn executing the `abort` call is itself
   cancelled.
2. Emits `AgentResponse::Exit`, so the TUI shuts down via the **graceful exit**
   path: the session is saved and `SessionEnd` hooks fire before the process
   ends and its background tasks die with it.

There is no hard `process::exit`, so nothing is lost. The model is expected to
prefer fixing a loop itself first and only call `abort` as a last resort — its
description says so explicitly.

## Capability axis: `affects_control_flow`

`abort` is **not** a filesystem tool. It declares
`Tool::affects_control_flow() = true`, an orthogonal capability axis to
[`ToolAccess`](access.md#toolaccess-tiers) (which classifies *filesystem
damage*). This axis classifies *process control*, and it — not `access()` — is
what gates the tool:

- **Sub-agent exclusion (unconditional).** A control-flow tool is excluded from
  *every* subagent profile, even the maximally permissive `INTERACTIVE` one. A
  spawned agent must never be able to tear down the whole program.
- **Broker bypass.** The filesystem permission broker does not prompt for it
  (an escape hatch that waits for approval is useless). Its `access()` is `Read`
  purely so the broker does not intercept it.

See [Capability axes](access.md#capability-axes). `abort` is the first
consumer of this axis; `requires_user` and `spawns_subagent` are the other two
non-filesystem axes.

## Why it exists

The harness deliberately keeps **no hard equality-guard abort** (the ADR-0009
equality guard was removed). The deterministic read-loop guard (ADR-0034) does
break *read* loops automatically — but only by injecting a non-terminating
nudge, and only for repeated reads; it does not stop the program, and it does
not cover every stuck state (a dangerous irreversible operation, a non-read dead
end). `abort` complements it as the **model-initiated** escape hatch for the
cases steering cannot resolve. The opt-in `hard_stop_rounds` total-round cap and
user `Esc` remain as the hard backstops.
