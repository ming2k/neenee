# User questions

The `ask_user` tool lets the model pause a turn and ask the user one or more
multiple-choice questions. This page describes the end-to-end mechanism:
how the request is declared, how the agent blocks until an answer arrives, and
how the TUI renders and returns the result.

## Why a dedicated tool

Before `ask_user`, neenee followed a strict "do not ask permission" rule in its
system prompt. The only interactive gate was the write-tool permission sheet,
which is a safety control, not a model-driven question. As tasks grew more
ambiguous, the model needed a structured way to resolve ambiguity without
falling back to free-text pleading or guessing.

A dedicated tool gives three advantages:

1. **Structured options** — the model supplies labels and descriptions, so the
   UI can render a consistent picker instead of parsing natural-language
   questions.
2. **Non-blocking architecture** — the tool uses the same oneshot-channel
   pattern as the permission broker, so the agent turn suspends cleanly and the
   UI remains responsive.
3. **Observable lifecycle** — every step (call, render, answer, result) flows
   through the normal `AgentEvent` stream, so the transcript records what was
   asked and what was chosen.

## Request shape

The model calls `ask_user` with a `questions` array. Each question contains
the display text, an optional header tag, an options array, and an optional
`multi_select` flag.

The TUI does not trust the model to provide a catch-all option. It appends an
**Other** option to every question and, when that option is highlighted, shows
a free-text field. This preserves user agency without requiring every model
prompt to remember to include an escape hatch.

## Execution flow

```text
 model ──tool call ask_user──► agent core
                                │
                                ▼
                    emit question-request event
                                │
                                ▼
              harness relay ──► TUI queues request
                                │
                                ▼
              TUI opens question modal
                                │
                                ▼
              user selects options / types Other
                                │
                                ▼
              TUI sends question reply
                                │
                ▼
              agent resolves blocked future
                                │
                                ▼
              tool result with selected labels
```

The blocking primitive is a oneshot channel. When the agent receives the tool
call, it stores the sender in a pending-question slot, emits the event, and
awaits the receiver. The TUI keeps the receiver alive by holding the request
in a queue; once the user answers, the TUI sends the reply, the agent resolves
the sender, and the tool future completes.

This design intentionally mirrors the permission broker
because that broker already solves the same problem: suspend a tool future,
queue the request, render a modal, and resume on user input. The key
difference is that questions are independent—cancelling or rejecting one
question does not cascade to other pending questions, unlike a permission
rejection which aborts the whole turn.

## TUI rendering

The question modal is a centered overlay. It shows one question at a time:

- Single-select questions use radio buttons (`○` / `●`).
- Multi-select questions use checkboxes (`[ ]` / `[x]`).
- Options are numbered 1–9 for direct keyboard access.
- The footer lists the available shortcuts.

When the **Other** option is highlighted, the modal renders an underlined text
field. Printable characters append to that field, and backspace removes the last
character. The typed text is returned instead of the literal `Other` label when
the user submits.

## Cancellation and interruption

If the user presses `Esc`, the TUI sends an empty answer. The agent returns a
text result explaining that no answer was provided, and the model decides how
to proceed.

If the user interrupts the turn (double `Esc`), the harness
rejects every pending question sender with `None`. Each blocked `ask_user`
future then resolves to the cancelled result.

## Planning

`ask_user` is `Read` access, so the main agent can use it freely to clarify
requirements before or during a task. Inside a subagent it is gated by the
profile's `allow_user_interaction` flag and the full-duplex channel
([ADR-0029](../../adr/0029-full-duplex-subagent-communication.md)): the default
`EXPLORE` profile is non-interactive, so a read-only research subagent that
needs clarification surfaces the request up to the main agent rather than
calling `ask_user` directly; the `INTERACTIVE` profile opts in and the
round-trip works through the handle.

## Sub-agents

`ask_user` also declares `requires_user`, so the built-in `EXPLORE` profile
excludes it from sub-agents. A sub-agent has no user reachable — its
question-request events are dropped by the dispatch tool's forwarder — so
admitting `ask_user` there would deadlock until the parent turn is cancelled.
Keeping the question with the parent (which *can* ask) is the contract; a
sub-agent that hits ambiguity returns it in its written answer instead. See
[Sub-agents → Tool admission](subagents.md#tool-admission) and
[ADR-0011](../../adr/0011-subagent-profiles.md).

## See also

- [How to ask the user a question](../../how-to/ask-the-user.md)
- [Built-in tools](../../reference/tools/index.md)
- [Tool rounds](turns-and-rounds.md)
