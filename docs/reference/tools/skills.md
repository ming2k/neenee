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
.neenee/skills/<name>/SKILL.md
~/.neenee/skills/<name>/SKILL.md
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

The skill registry (`crates/neenee-core/src/skills/`) discovers skills from, in
priority order (later sources override earlier ones):

1. Bundled system skills (`~/.neenee/skills/.system/`).
2. Remote skill repositories configured in `config.toml`, cached under
   `~/.cache/neenee/skills/`.
3. User-global skills: `~/.neenee/skills/`, `~/.agents/skills/`,
   `~/.claude/skills/`, `~/.kimi-code/skills/`.
4. Extra local paths configured in `config.toml`.
5. Project-repo skills: `.neenee/skills/`, `.agents/skills/`, `.claude/skills/`,
   `.kimi-code/skills/`.

The catalog is built by `build_skills_index` and injected into the system
prompt by `Agent::build_system_prompt`. Skills whose names are mentioned in a
user message are auto-loaded by `Agent::inject_implicit_skills` when their
policy allows implicit invocation.
