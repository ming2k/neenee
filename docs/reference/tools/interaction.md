# Interaction tools

Tools for the agent to manage its own state and to query the user mid-task.
All are `Read` and bypass the permission broker. `ask_user` lives in
`neenee-tools`; `progress_update`, `todo`, and `todo_update` live in
`neenee-agent`.

### `ask_user`

`AskUserTool` overrides `requires_user() = true`, so it is excluded from every
sub-agent profile — a sub-agent has no user reachable to answer it. See
[Sub-agent admission](../../explanation/agent-design/subagents.md#tool-admission)
and [User questions](../../explanation/agent-design/user-questions.md).

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `questions` | array | yes | 1–5 questions |

Each question: `{ "header"?: string, "question": string, "options": Option[], "multi_select"?: boolean }`.
Each option: `{ "label": string, "description"?: string }`. The model should put the recommended option first and suffix its label with `(Recommended)`. The TUI always appends an "Other" free-text option so the user is not forced into the model's choices.

### `progress_update`

Report a very short current work status for the TUI activity bar and Activity
modal. The status is model-authored, persists through transport phases inside
the active turn, and clears when the turn ends. The tool is controlled by
`agent.progress_updates.enabled`.

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `summary` | string | yes | Collapsed to one line and truncated to `agent.progress_updates.max_chars` |

### `todo`

Full-replace the unified task list — the single source of truth for "what is
left to do," shown in the [Activity](../tui/modals.md) modal and persisted
across restarts. The tool reconciles the desired list against the current
one, preserving item identity when content is unchanged, so re-sending the same
steps does not reset their timestamps. See
[ADR-0020](../../adr/0020-unified-task-list.md).

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `items` | array | yes | Max 50 items; at most one `in_progress` |

Each item: `{ "content": string, "status": "pending" | "in_progress" | "completed" | "cancelled" }`.

### `todo_update`

Surgically update the status of one or more existing items without re-sending
the whole list. Prefer this over `todo` when marking progress on a single step.

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `key` | string | yes | 1-based position (e.g. `"1"`) or case-insensitive content substring (all matches update) |
| `status` | enum: `pending` / `in_progress` / `completed` / `cancelled` | yes | New status for the matched item(s) |
