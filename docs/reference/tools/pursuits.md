# Pursuit tools

`get_pursuit`, `start_pursuit`, and `complete_pursuit` are force-injected by `Agent::new`
from `crates/neenee-core/src/pursuits/tools.rs` (any externally supplied copies
are stripped first). They share a `GoalToolContext` carrying the session/thread
id and the `GoalService`, which persists pursuit state in SQLite.

`get_pursuit` is `Read` and bypasses the permission broker; `start_pursuit` and
`complete_pursuit` are `Write`. Both `Write` tools override `permission_label` and
`permission_description` so the prompt header reads `Create pursuit` /
`Update pursuit` and the body describes the effect of the call rather than the
model-facing instructions encoded in `Tool::description`.

A pursuit is `{ objective, is_complete }` — there is no status machine and no
checklist. The `/pursue` stop-gate (not a tool) drives the agent toward the
objective; these tools only read/set/complete it. See
[Pursuits and the pursue stop-gate](../../explanation/agent-design/pursuits.md).

### `get_pursuit`

No parameters. Returns the current pursuit as JSON — objective and completion
flag — or `{"pursuit": null}` when none is set.

### `start_pursuit`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `objective` | string | yes | The objective to start pursuing |

Starts a new active pursuit (replaces any existing one). The model is instructed
to call this only when the user or developer instructions explicitly ask for a
pursuit, never inferred from an ordinary task.

### `complete_pursuit`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `status` | enum | yes | `complete` |

Marks the active pursuit `complete` (objective achieved, no work remaining). This
is the tool-based completion path; the `[NEENEE_PURSUIT_COMPLETE]` marker is the
in-turn path a running `/pursue` uses.

See [Pursuits and the pursue stop-gate](../../explanation/agent-design/pursuits.md)
for the pursuit primitive.
