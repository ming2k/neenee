# Goal tools

`get_goal`, `create_goal`, `update_goal`, and `goal_checklist` are force-injected
by `Agent::new` from `crates/neenee-core/src/goals/tools.rs` (any externally
supplied copies are stripped first). They share a `GoalToolContext` carrying
the session/thread id and the `GoalService`, which persists goal state in
SQLite; `goal_checklist` also shares the agent's live
`Arc<Mutex<Option<Goal>>>`.

`get_goal` and `goal_checklist` are `Read` and bypass the permission broker;
`create_goal` and `update_goal` are `Write`. Both `Write` tools override
`permission_label` and `permission_description` so the prompt header reads
`Create goal` / `Update goal` and the body describes the effect of the call
rather than the model-facing instructions encoded in `Tool::description`.

### `get_goal`

No parameters. Returns the current goal as JSON — objective, completion flag,
and checklist — or `{"goal": null}` when none is set.

### `create_goal`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `objective` | string | yes | The objective to start pursuing |

Starts a new active goal. The model is instructed to call this only when the
user or developer instructions explicitly ask for a goal, never inferred from
an ordinary task.

### `update_goal`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `status` | enum | yes | `complete` |

Marks the active goal `complete` (objective achieved, no work remaining).
Pause, resume, and budget/usage limits are user-controlled, not reachable here.

### `goal_checklist`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `items` | array | yes | Max 50 items |

Each item: `{ "content": string, "status": "pending" | "in_progress" | "completed" | "cancelled" }`.

Hard rules enforced in `crates/neenee-core/src/goals/tools.rs`:

- Empty `content` rejected.
- At most one `in_progress` item.
- A non-empty checklist cannot be replaced with an empty list; each item must
  receive a terminal `completed` or `cancelled` status.

See [Goals](../../explanation/agent-design/goals.md) for the goal primitive.
