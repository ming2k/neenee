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

The model calls `ask_user` with a `questions` array. Each question contains the
display text, an optional header tag, an options array, and an optional
`multi_select` flag. The schema lives in `crates/neenee-tools/src/lib.rs`.

The TUI does not trust the model to provide a catch-all option. It appends an
**Other** option to every question and, when that option is highlighted, shows
a free-text field. This preserves user agency without requiring every model
prompt to remember to include an escape hatch.

## Execution flow

```text
 model ──ToolCall ask_user──► agent core
                                │
                                ▼
                    AgentEvent::UserQuestionRequest
                                │
                                ▼
              harness relay ──► TUI queues request
                                │
                                ▼
              TUI opens Modal::Question
                                │
                                ▼
              user selects options / types Other
                                │
                                ▼
              AgentRequest::UserQuestionReply
                                │
                                ▼
              agent resolves oneshot receiver
                                │
                                ▼
              ToolResult with selected labels
```

The blocking primitive is a `tokio::sync::oneshot` channel. When the agent
receives the tool call, it creates a `UserQuestionRequest`, stores the sender
in `Agent.ask_user.pending`, emits the event, and awaits the receiver. The TUI
keeps the receiver alive by holding the request in a queue; once the user
answers, the TUI sends `AgentRequest::UserQuestionReply`, the agent removes the
sender, and the tool future completes.

This design intentionally mirrors the permission broker (`Agent.permissions`)
because that broker already solves the same problem: suspend a tool future,
queue the request, render a modal, and resume on user input. The key
difference is that questions are independent—cancelling or rejecting one
question does not cascade to other pending questions, unlike a permission
rejection which aborts the whole turn.

## TUI rendering

The question modal is a centered overlay (`draw_question_modal` in
`crates/neenee-cli/src/tui/render/overlays.rs`). It shows one question at a time:

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

If the user interrupts the turn (`Ctrl+C` or the equivalent), the harness calls
`Agent::reject_pending_user_questions`, which drops every pending sender with
`None`. Each blocked `ask_user` future then resolves to the cancelled result.

## Plan mode

`ask_user` is marked `ToolAccess::Read` and is explicitly allowed in Plan mode.
Clarifying requirements is a read-only activity, so planners can use it to
resolve ambiguity before any implementation begins.

## See also

- [How to ask the user a question](../../how-to/ask-the-user.md)
- [Built-in tools](../../reference/tools.md)
- [Tool rounds](tool-rounds.md)
