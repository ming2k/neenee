# Filesystem tools

Read and mutate files and directory listings. `read_file` / `grep` / `glob` /
`list_dir` are `Read`; `write_file` / `edit_file` are `Write` (Plan-exempt for
paths under `.neenee/plans/`). Source: `crates/neenee-tools/src/lib.rs`.

### `read_file`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | yes | — | File path |
| `offset` | integer | no | — | 1-based start line |
| `limit` | integer | no | — | Max lines |

### `write_file`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `path` | string | yes | File path |
| `content` | string | yes | Full content; overwrites |

### `edit_file`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `path` | string | yes | File path |
| `old_string` | string | yes | Must exist verbatim |
| `new_string` | string | yes | Replacement text |

### `grep`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `pattern` | string | yes | — | Regex |
| `path` | string | no | `.` | Search root |
| `ext` | string | no | — | File extension filter |

Backed by ripgrep.

### `glob`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `pattern` | string | yes | — | Glob, e.g. `**/*.rs` |
| `path` | string | no | `.` | Search root |

Capped at `GLOB_MAX_RESULTS = 200` (`crates/neenee-tools/src/lib.rs`).

### `list_dir`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | no | `.` | Directory |
| `pattern` | string | no | — | Optional glob |
| `recursive` | boolean | no | `false` | Recurse |
| `max_results` | integer | no | `100` | Cap |
