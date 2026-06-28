# Project tools

Scaffold new projects and initialise neenee config. Both are `Write`. Source:
`crates/neenee-tools/src/project.rs`.

## `create_project`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `name` | string | yes | — | Project name |
| `type` | enum | yes | — | `rust`, `node`, `python`, `go`, `generic` |
| `path` | string | no | `.` | Parent directory |
| `git` | boolean | no | `true` | `git init` |
| `neenee` | boolean | no | `false` | Scaffold `.neenee/` |

Permission scope is `{path}/{name}` or `*`.

## `init_config`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | no | `.` | Target directory |

Idempotent; existing files are never overwritten. Permission scope is the
`path` argument or `.`.
