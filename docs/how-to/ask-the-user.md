# How to ask the user a question during a task

Use the `ask_user` tool when the agent needs a decision, clarification, or
preference from the user before continuing. The tool blocks the turn, renders a
modal in the TUI, and returns the selected option labels to the model.

## When to use `ask_user`

Ask the user only when the choice materially changes the outcome and cannot be
inferred from the repository or the request:

- The request is ambiguous in a way that changes the result.
- Multiple valid approaches exist and the trade-offs matter.
- The next action is risky, destructive, or hard to reverse.
- You need a user preference before generating content (style, framework,
  target audience, etc.).

Do **not** use `ask_user` for permission to run an ordinary tool—the existing
permission sheet already handles write-tool approval. Do **not** ask "Is this
plan okay?" or "Should I proceed?"; take the most reasonable action and mention
what you did.

## Construct a question

A tool call contains one or more questions. Each question needs:

- `question`: the full text the user sees.
- `options`: an array of `{ "label": string, "description"?: string }`.
- `multi_select` (optional, default `false`): whether the user may pick more
  than one option.
- `header` (optional): a short tag shown above the question.

Example:

```json
{
  "questions": [
    {
      "header": "Error handling",
      "question": "Which error handling style should this crate use?",
      "options": [
        { "label": "anyhow (Recommended)", "description": "Ergonomic dynamic errors" },
        { "label": "thiserror", "description": "Structured enum-based errors" }
      ],
      "multi_select": false
    }
  ]
}
```

Follow these conventions:

1. Provide 2–4 options. Fewer makes the question look forced; more overwhelms
   the UI.
2. Put the recommended option first and suffix its label with `(Recommended)`.
3. Keep labels short and stable; the label is what the model receives back.
4. Use `description` for the one-line context that helps the user decide.

## The automatic "Other" option

The TUI appends an **Other** option to every question automatically. Selecting
it lets the user type a free-form answer. If the user submits Other without
typing anything, the tool result contains the literal string `Other`.

## Handle the answer

The tool result is a JSON array with one inner array per question. Each inner
array contains the selected option labels:

```text
User answered the question(s). Selected option labels:
[
  [
    "thiserror"
  ]
]
```

For `multi_select: true`, the inner array may contain several labels. Parse the
result and continue the task according to the user's choice.

## Cancelled questions

If the user presses `Esc`, the tool returns:

```text
User cancelled the question; no answer was provided.
```

Treat this as a declined request. Either fall back to a safe default, explain
why you need an answer, or stop the current branch of work.

## See also

- [Built-in tools](../reference/tools.md)
- [User question mechanism](../explanation/user-questions.md)
