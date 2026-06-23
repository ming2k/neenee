# Bundled skills

This directory tree is embedded into the `neenee` binary at compile time
(via `include_dir!` in `crates/neenee-agent/src/skills/bundled.rs`) and
surfaced as `SkillScope::System` skills — the lowest-priority source in the
discovery cascade.

## Layout

Each subdirectory is one skill, identified by a `SKILL.md`:

```text
bundled/
  <skill-name>/
    SKILL.md          # required: frontmatter + body
    reference.md      # optional: extra files referenced by the body
    ...
```

## Why compile-time embed (not a runtime directory)

The previous design placed bundled skills at `~/.neenee/skills/.system/`,
which had three problems:

1. It violated the XDG Base Directory Specification by writing into the
   user's home directory.
2. Read-only shipped data sat inside a user-writable tree, so any install
   step was fragile and the content could be accidentally edited.
3. The `.system` segment being hidden (`.system`) was load-bearing — it
   leaned on `discovery.rs`'s hidden-directory filter to avoid double
   counting as a user skill. That coupling was a design smell.

Embedding at compile time removes all three: zero I/O at startup, content
is always read-only, and the `System` scope is expressed in the type, not
in a directory naming convention.

## Adding a bundled skill

1. Create `bundled/<name>/SKILL.md` here.
2. Write frontmatter (`name`, `description`, ...) and a body.
3. Rebuild — no inventory, manifest, or registration step is required;
   `bundled::discover()` walks the embedded tree at startup.
