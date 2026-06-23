# Skills and `use_skill`

Skills are not tools, but the `use_skill` tool loads them.

### `use_skill`

| Parameter | Type | Required | Notes |
|-----------|------|----------|-------|
| `name` | string | yes | Skill name from frontmatter |

`UseSkillTool` (`crates/neenee-agent/src/skills/tools.rs`) loads the skill body
into the conversation. `Read`; bypasses the permission broker.

## Skill format

A skill is a Markdown file with YAML frontmatter, conventionally named
`SKILL.md` inside a skill directory:

```text
.neenee/skills/<name>/SKILL.md                          # project-local
$XDG_DATA_HOME/neenee/skills/<name>/SKILL.md            # user-global
```

```text
---
name: my-skill
description: When to invoke this skill
short-description: Short help
version: "1.0.0"
tags: [rust]
policy:
  allow_implicit_invocation: true
dependencies:
  tools:
    - type: mcp
      value: rust-analyzer
---
Skill body injected into the context on demand.
```

## Discovery

The skill registry (`crates/neenee-agent/src/skills/discovery.rs`) discovers
skills from, in priority order (later sources override earlier ones):

1. **Bundled system skills** — compile-time-embedded under
   `crates/neenee-agent/skills/bundled/`; baked into the binary, never on
   disk. (See ADR-0013.)
2. **Remote skill repositories** configured under `[skills] urls` in
   `config.toml`, cached under `$XDG_CACHE_HOME/neenee/skills/remote/`.
3. **User-global skills (XDG)** — `$XDG_DATA_HOME/neenee/skills/`
   (`~/.local/share/neenee/skills/` on Linux by default).
4. **External user-global formats** — `~/.agents/skills/`, `~/.claude/skills/`,
   `~/.kimi-code/skills/` (someone else's app convention).
5. **Legacy user-global skills** — `~/.neenee/skills/`. Deprecated pre-XDG
   fallback; emits a one-time warning on load directing you to move files to
   the XDG location. Will be removed in a future release.
6. **Extra local paths** configured under `[skills] paths` in `config.toml`.
7. **Project-repo skills** — `.neenee/skills/`, `.agents/skills/`,
   `.claude/skills/`, `.kimi-code/skills/` in the project root (highest
   priority).

All user-level paths resolve through the central `Dirs` layer
(`crates/neenee-store/src/paths.rs`) and honour the standard XDG overrides
(`$XDG_DATA_HOME`, `$XDG_CACHE_HOME`) plus the app-specific overrides
(`$NEENEE_DATA_DIR`, `$NEENEE_CACHE_DIR`). See [Paths](../paths.md) for
the full override stack and [Persistence and the XDG
layout](../../explanation/persistence.md) for the conceptual model.

The catalog is built by `build_skills_index` and injected into the system
prompt by `Agent::build_system_prompt`. Skills whose names are mentioned in a
user message are auto-loaded by `Agent::inject_implicit_skills` when their
policy allows implicit invocation.
