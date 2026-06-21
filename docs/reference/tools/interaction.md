# Interaction tools

Tools for the agent to manage its own state and to query the user mid-task.
Both are `Read` and bypass the permission broker. Source:
`crates/neenee-tools/src/lib.rs`.

### `ask_user`

`AskUserTool` overrides `requires_user() = true`, so it is excluded from every
sub-agent profile — a sub-agent has no user reachable to answer it. See
[Sub-agent admission](../../explanation/agent-design/subagents/admission.md)
and [User questions](../../explanation/agent-design/user-questions.md).

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `questions` | array | yes | 1–5 questions |

Each question: `{ "header"?: string, "question": string, "options": Option[], "multi_select"?: boolean }`.
Each option: `{ "label": string, "description"?: string }`. The model should put the recommended option first and suffix its label with `(Recommended)`. The TUI always appends an "Other" free-text option so the user is not forced into the model's choices.

### `todo`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `items` | array | yes | Max 50 items |

Each item: `{ "content": string, "status": "pending" | "in_progress" | "completed" }`.
