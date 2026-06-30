# Filesystem tools

Read and mutate files and directory listings. `read_file` / `read_image` /
`grep` / `glob` / `list_dir` are `Read`; `write_file` / `edit_file` are
`Write`. Source: `crates/neenee-tools/src/lib.rs`.

## `read_file`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | yes | — | File path |
| `offset` | integer | no | — | 1-based start line |
| `limit` | integer | no | — | Max lines |

## `read_image`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | yes | — | Image file path |

Reads an image file (PNG, JPEG, GIF, WebP) and delivers it inline so a
vision-capable model can see it. Large images are auto-resized to a sensible
resolution before sending. For plain-text files use `read_file` instead.

The image is returned as a structured `ToolOutput::Image` and delivered to the
model out-of-band: the tool result message carries a short text placeholder,
and the harness injects the actual image into a follow-up user-role message.
This mirrors how opencode lowers images out of tool results for OpenAI Chat
Completions providers (whose tool messages only accept string content), so it
works across kimi / GLM / OpenAI / Gemini.

## `write_file`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `path` | string | yes | File path |
| `content` | string | yes | Full content; overwrites |

## `edit_file`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `path` | string | yes | File path |
| `old_string` | string | yes | Must exist verbatim |
| `new_string` | string | yes | Replacement text |

## `grep`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `pattern` | string | yes | — | Regex |
| `path` | string | no | `.` | Search root |
| `ext` | string | no | — | File extension filter |
| `context` | integer | no | `0` | Lines of context per match (clamped to 10) |

Backed by ripgrep. Output is capped globally (≈200 lines / 32 KB) with a
truncation notice; `--max-count` bounds matches per file at 50.

## `glob`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `pattern` | string | yes | — | Glob, e.g. `**/*.rs` |
| `path` | string | no | `.` | Search root |

Capped at `GLOB_MAX_RESULTS = 200` (`crates/neenee-tools/src/lib.rs`).

## `list_dir`

| Parameter | Type | Required | Default | Notes |
|-----------|------|----------|---------|-------|
| `path` | string | no | `.` | Directory |
| `pattern` | string | no | — | Optional glob |
| `recursive` | boolean | no | `false` | Recurse |
| `max_results` | integer | no | `100` | Cap |
