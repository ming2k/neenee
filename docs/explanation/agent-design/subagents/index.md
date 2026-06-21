# Sub-agents

The `task` tool spawns an isolated child agent to investigate a sub-question
and return a written answer. The parent agent stays in control of all writes
and of any questions to the user. This directory explains the mechanism; for
the tool's parameters and access class, see
[`task`](../../reference/tools/task.md).

## Why a sub-agent tool

A single agent turn accumulates context: every file read, every grep result,
every tool round stays in the transcript. For a large investigation that
touches many unrelated corners of the codebase, one of two things happens —
either the context fills with material only loosely related to the final
answer, or the model spends turns re-reading things it already saw. A
sub-agent gives the model a way to delegate the exploration:

1. **Context isolation.** The sub-agent runs with a fresh two-message history
   (its task prompt plus the system prompt). Its tool rounds never enter the
   parent's transcript; only its final summary does.
2. **Read-only and non-interactive by construction.** The sub-agent receives
   only the tools its profile admits, so it cannot mutate the workspace, never
   triggers the permission broker, and can never block on a question to the
   user (it has no user to reach). See [Profiles](profiles.md) and
   [Admission](admission.md).
3. **Parallelizable investigation.** The model can dispatch several `task`
   calls to map different parts of a problem, then act on the synthesized
   findings.

## The `task` tool

The `task` tool is the one built-in tool whose result is not a single value
but a streamed investigation. It takes a short description and a prompt, both
required, and returns a payload carrying the sub-agent's summary, its full
transcript, and its token usage. The parent persists that transcript as the
tool step's children, so `/resume` rebuilds the nested view later.

Because the sub-agent's progress is interesting in real time (not just its
final answer), the tool streams live rather than blocking until completion:
every token and tool round the child produces is relayed to the parent TUI as
it happens. Input validation rejects only non-JSON or empty-after-trim fields;
the length hint on the description is a model-facing nudge, not an enforced
bound.

## Isolation model

The sub-agent shares exactly one thing with the parent — the model provider —
and nothing else:

| Concern | Shared? | How |
|---------|---------|-----|
| Provider | Yes | The same provider connection |
| Conversation history | No | A fresh system + task prompt |
| Tools | Snapshot, profile-filtered | The tools the bound [profile](profiles.md) admits |
| Goal state | No | An empty in-memory goal store |
| Plan state, mode | No | Build mode, no active plan |
| Skills | No | No loaded skills |
| Cancellation token | No | A fresh, independent token |
| Session persistence | No | The sub-agent is never persisted |

The filesystem is implicitly shared because the sub-agent inherits the process
working directory, but its profile admits no file-write tools, so it cannot
mutate files.

## In this section

- [Profiles](profiles.md) — the `SubagentProfile` primitive, and why there are
  two built-in roles (`EXPLORE`, `VERIFY`).
- [Admission](admission.md) — how `ToolPolicy::admits` decides which tools a
  sub-agent may use, and why `ask_user` is excluded.
- [Runtime](runtime.md) — event streaming, the TUI zoom view, failure and
  cancellation, and Plan-mode interaction.
- [Plan verification](plan-verification.md) — the `verify_plan_execution`
  scenario, the second sub-agent role.

## See also

- [`task`](../../reference/tools/task.md) — parameter reference.
- [Plan mode](../plan-mode.md) — `task` in Plan mode, and plan verification.
- [Tool rounds](../tool-rounds.md) — the round trip the sub-agent runs internally.
- [Goals](../goals.md) — how sub-agent token cost flows up to a parent goal.
- [Harness architecture](../harness.md) — the safety bounds that bound a
  sub-agent turn.
