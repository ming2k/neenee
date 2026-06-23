# 0013. Skills & commands: XDG paths + compile-time-embedded bundled skills

- **Status:** Accepted
- **Date:** 2026-06-23

## Context

Skill and slash-command discovery (`crates/neenee-agent/src/skills/discovery.rs`,
`crates/neenee-tools/src/commands.rs`, `crates/neenee-agent/src/skills/remote.rs`)
resolved user-global paths by hard-coding `dirs::home_dir().join(".neenee/...")`.
This violated the XDG Base Directory Specification and was inconsistent with
the rest of the codebase: `neenee-store/src/paths.rs` already exposes a
central `Dirs` abstraction that honours `$XDG_DATA_HOME`, `$XDG_CACHE_HOME`,
`$XDG_STATE_HOME`, the `NEENEE_*_DIR` app overrides, and the `--data-dir` /
`--cache-dir` CLI flags — and other subsystems (sessions, blobs, embeddings,
provider usage, locks) already route through it.

Two concrete smells sat on top of that mismatch:

1. **`~/.neenee/skills/.system/`** was the lowest-priority "bundled system
   skills" source. Putting read-only shipped data inside a user-writable
   directory required an install/sync step to populate, allowed accidental
   edits, and leaned on `.system` being a *hidden* directory so that
   `is_inside_hidden_dir` (`discovery.rs`) would skip it during the
   user-scope scan to avoid double-counting. The hidden-name convention was
   load-bearing and undocumented.

2. **`~/.neenee/skills/`** and **`~/.neenee/commands/`** bypassed
   `--data-dir`, `$XDG_DATA_HOME`, and `NEENEE_DATA_DIR`, so sandboxed,
   containerised, or multi-prefix setups could not redirect skill storage.

## Decision

1. **Route user-global skills and commands through `Dirs`.** Add
   `Dirs::user_skills_dir()` → `$XDG_DATA_HOME/neenee/skills` and
   `Dirs::user_commands_dir()` → `$XDG_DATA_HOME/neenee/commands`. Both
   inherit the existing `Dirs` override stack (CLI > `NEENEE_*` > `XDG_*` >
   native > home fallback). Remove the misleadingly-named
   `Dirs::local_skills_dir()` (which appended an unjustified `/local`
   segment).

2. **Embed bundled system skills at compile time.** Add
   `crates/neenee-agent/src/skills/bundled.rs` backed by `include_dir!`
   against `crates/neenee-agent/skills/bundled/`. `bundled::discover()`
   walks the embedded tree, parses each `SKILL.md` via the shared
   `metadata::parse_skill_from_str`, and returns skills already typed as
   `SkillScope::System`. No filesystem location, no install step, no
   `.system` hidden-name hack. `discover_all` calls it before any
   filesystem source.

3. **Route the remote-skill cache through `Dirs`.**
   `remote::remote_cache_root()` now returns `Dirs::remote_skills_cache()`
   → `$XDG_CACHE_HOME/neenee/skills/remote`, instead of hand-rolling
   `dirs::cache_dir().or(home).join("neenee/skills/remote")`. Consolidates
   two ad-hoc implementations of the same path.

4. **Scan legacy `~/.neenee/skills/` and `~/.neenee/commands/` as deprecated
   fallbacks.** Detection (`has_discoverable_skills` /
   `has_markdown_files`) gates a one-time `tracing::warn!` per process that
   directs the user to the XDG location. The legacy source is registered
   *before* the XDG source in the priority cascade, so an XDG copy
   overrides a legacy copy on name collision — there is no silent
   fork. This preserves the "upgrades never lose user content" invariant
   without a risky auto-migration.

5. **Extract `metadata::parse_skill_from_str`** as the shared body of
   `parse_skill_file` so the on-disk path and the in-memory embed path
   cannot drift in their frontmatter interpretation.

External skill directories (`~/.agents/skills/`, `~/.claude/skills/`,
`~/.kimi-code/skills/`) and the project-local `.neenee/skills/` /
`.neenee/commands/` are unchanged: they belong to other applications or to
the project working tree, neither of which neenee controls.

## Alternatives considered

- **Auto-migrate `~/.neenee/skills/*` into `$XDG_DATA_HOME/neenee/skills/`
  on first run.** Rejected: copying user-authored content without consent
  is a footgun, and the source tree may contain files that aren't skills
  (notes, backups) that the user does not want duplicated. A deprecation
  warning + manual migration puts the user in charge.
- **`#[allow(deprecated)]` silent dual-scan without a warning.** Rejected:
  silent fallback is what created the `~/.neenee/skills/.system/` situation
  in the first place. A user who runs `rm -rf ~/.neenee` and is surprised
  to find their skills still loaded would have no signal that the layout
  changed.
- **Ship bundled skills as a read-only data dir under
  `$XDG_DATA_HOME/neenee/bundled-skills/` (install-time copy).** Rejected
  vs. compile-time embed: an install step is more moving parts (and a
  package-builder concern per platform), the data is genuinely read-only
  forever, and `include_dir!` costs zero runtime I/O and one small
  dependency.
- **Hand-roll the frontmatter parser inside `bundled.rs` instead of
  extracting `parse_skill_from_str`.** Rejected: skill schema
  interpretation must have exactly one location, or the disk and embedded
  paths will drift.

## Consequences

**Positive**

- User skills / commands now answer to the same `--data-dir` /
  `$XDG_DATA_HOME` / `NEENEE_DATA_DIR` overrides as the rest of the store.
- Bundled skills are trivially correct: read-only, no I/O, no install step,
  no hidden-directory coupling.
- `SkillScope::System` is now expressed in the type system rather than in
  a `.system` directory naming convention.
- The remote-skill cache respects `$XDG_CACHE_HOME` / `--cache-dir`, so
  `rm -rf $XDG_CACHE_HOME/neenee` is the single sweep that flushes caches.
- Frontmatter interpretation has one home (`metadata::parse_skill_from_str`).

**Negative / neutral**

- One new dependency (`include_dir = "0.7"`) on `neenee-agent`.
- Users with pre-XDG content at `~/.neenee/skills/` or `~/.neenee/commands/`
  will see a deprecation warning on every startup until they migrate. The
  path is *not* removed in this ADR; a later ADR will retire it once the
  warning has shipped in a release or two.
- `bundled::discover()` synthesises `source` / `root` paths
  (`bundled/<name>/SKILL.md`) that do not exist on disk. Anyone surfacing
  these paths in UI should treat them as identifiers, not file URLs.

**Migration for users**

```sh
mv ~/.neenee/skills/*   $XDG_DATA_HOME/neenee/skills/   2>/dev/null || true
mv ~/.neenee/commands/* $XDG_DATA_HOME/neenee/commands/ 2>/dev/null || true
rmdir ~/.neenee/skills ~/.neenee/commands ~/.neenee     2>/dev/null || true
```

Where `$XDG_DATA_HOME` defaults to `~/.local/share` on Linux.

## References

- `crates/neenee-store/src/paths.rs` — `Dirs`, `user_skills_dir`,
  `user_commands_dir`, `remote_skills_cache`.
- `crates/neenee-agent/src/skills/discovery.rs` — source cascade, legacy
  fallback gating.
- `crates/neenee-agent/src/skills/bundled.rs` — compile-time embed.
- `crates/neenee-agent/src/skills/metadata.rs` — shared
  `parse_skill_from_str`.
- `crates/neenee-agent/src/skills/remote.rs` — cache routed through `Dirs`.
- `crates/neenee-tools/src/commands.rs` — XDG + legacy fallback.
- [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/latest/)
- ADR-0005 (strict layering) — `neenee-tools` now depends on
  `neenee-store` for `paths::get()`. The new edge keeps the strict layering
  intact: `neenee-store` does not depend on `neenee-tools`, so no cycle is
  introduced.
- ADR-0014 — codifies the unified XDG persistence architecture as
  project-wide policy (this ADR is the skills-specific application).
