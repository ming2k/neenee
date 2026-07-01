# Project tools

Initialise neenee config in a new or existing project. `Write`. Source:
`crates/neenee-tools/src/project.rs`.

## `init_config`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | no | `.` | Target directory |

Idempotent; existing files are never overwritten. Permission scope is the
`path` argument or `.`.
