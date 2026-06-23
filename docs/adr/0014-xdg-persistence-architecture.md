# 0014. Unified XDG persistence architecture

- **Status:** Accepted
- **Date:** 2026-06-23

## Context

neenee writes a growing set of files to disk: configuration, session
histories, content-addressed blobs, per-project embeddings, permission
caches, slash-command history, model-usage telemetry, advisory locks,
remote-skill cache, plus user-authored skills and commands. The natural
questions — *where does this file live, what is it allowed to override it,
and what is safe to delete* — must have one answer per file, and the answer
must be derivable from a small set of rules rather than looked up ad hoc.

ADR-0013 fixed the skills subsystem, which had been bypassing the central
`Dirs` layer (`crates/neenee-store/src/paths.rs`) by hard-coding
`dirs::home_dir().join(".neenee/...")`. That exposed a wider gap: the `Dirs`
layer was the *de facto* standard for storage, but no ADR had declared it
the *policy*. Every new subsystem therefore risked the same shortcut.

Concretely, three classes of mistake were possible:

1. **Wrong XDG category.** Treating rebuildable caches as data, program
   state as configuration, or user-authored content as ephemeral. The XDG
   Base Directory Specification separates these for good operational reasons
   (different backup, snapshot, and cleanup semantics).
2. **Bypassing overrides.** Hard-coding `~/.neenee/...` ignores
   `$XDG_DATA_HOME`, `$XDG_CONFIG_HOME`, the `$NEENEE_*_DIR` app-specific
   overrides, and the `--data-dir` / `--cache-dir` CLI plumbing reserved
   for `PathsOverride`. Sandboxed, containerised, and multi-prefix setups
   silently break.
3. **Inconsistent override precedence.** Each subsystem inventing its own
   resolution order makes the user unable to predict which knob wins.

## Decision

1. **`Dirs` (`crates/neenee-store/src/paths.rs`) is the single point of
   truth for every path neenee writes outside the project working tree.**
   New persistent locations must be added as methods on `Dirs`, never
   assembled inline at the call site.

2. **Each path belongs to exactly one XDG category, chosen by what the
   file *is*, not where it happens to be read from:**

   | Category | What lives here | Lossy? |
   |----------|-----------------|--------|
   | **Config** (`$XDG_CONFIG_HOME`) | Files the user edits by hand (`config.toml`) | Lossy — user-owned |
   | **Data** (`$XDG_DATA_HOME`) | Persistent, program-generated, must survive restart (sessions, blobs, goals, project buckets, user skills/commands) | Lossy |
   | **State** (`$XDG_STATE_HOME`) | Persistent, program-generated, rebuildable (history, telemetry, locks when runtime is unavailable) | Rebuildable |
   | **Cache** (`$XDG_CACHE_HOME`) | Derived, deletable, repopulated on demand (remote-skill cache) | Safe to delete |
   | **Runtime** (`$XDG_RUNTIME_DIR`, Linux only) | Ephemeral per-login (cross-process locks) | Ephemeral |

3. **Override precedence is fixed and identical for every category**
   (highest first):
   1. CLI flag (`--config-dir`, `--data-dir`, `--state-dir`, `--cache-dir`)
      via `PathsOverride`.
   2. App-specific env var (`NEENEE_CONFIG_DIR`, `NEENEE_DATA_DIR`,
      `NEENEE_STATE_DIR`, `NEENEE_CACHE_DIR`).
   3. Standard XDG env var (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
      `XDG_STATE_HOME`, `XDG_CACHE_HOME`). Relative values are ignored
      (per spec).
   4. Native per-OS defaults via the `directories` crate.
   5. `$HOME/.config`, `$HOME/.local/share`, `$HOME/.local/state`,
      `$HOME/.cache` fallback when even native resolution fails.
   6. Current working directory as the absolute last resort (never
      panics).

4. **The project working tree is out of scope for XDG.** Project-local
   artefacts (`.neenee/skills/`, `.neenee/commands/`, `.neenee/plans/`)
   live under the project root and intentionally travel with the
   repository. They are documented separately in
   [the paths reference](../reference/paths.md).

5. **External conventions are tolerated, not endorsed.** neenee reads
   `~/.agents/skills/`, `~/.claude/skills/`, `~/.kimi-code/skills/`,
   `~/.agents/commands/`, etc. because those locations belong to other
   applications and moving them is not neenee's call. They are read-only
   discovery sources, never write targets.

6. **Legacy pre-XDG paths (`~/.neenee/`) are deprecated.** They are
   scanned as low-priority fallbacks with a one-time `tracing::warn!`
   directing the user to the XDG location. A future ADR will retire them
   once the deprecation has shipped in at least one release.

7. **`paths::get()` is the only production entry point.** It reads the
   process-wide `Dirs` (installed by `set_default` in startup, or lazily
   via `Dirs::system()` in library-only callers). Tests install an
   override through `set_test_default` so they never touch real XDG
   state.

## Alternatives considered

- **Per-subsystem path resolution.** Rejected: the historical outcome is
  the bug ADR-0013 fixed. Centralisation is the value; spreading the
  policy across N callers re-creates the original inconsistency.

- **A single `~/.neenee/` "everything" directory.** Rejected: collapses
  the XDG category semantics that real-world users rely on (separate
  backup and cleanup for config vs data vs cache; ephemeral runtime
  directories; per-OS native conventions on macOS and Windows).

- **Auto-migration of legacy `~/.neenee/` into XDG on first run.**
  Rejected: copying user-authored content without consent risks
  duplicating non-skill files (notes, backups). A deprecation warning
  plus documented `mv` commands puts the user in charge. See ADR-0013's
  migration snippet.

- **Drop the `directories` crate and read only `$XDG_*_HOME`.** Rejected:
  macOS and Windows users get sensible native defaults
  (`~/Library/Application Support/neenee`, `%APPDATA%\neenee`) through
  `directories`. The crate is small, maintained, and the only thing it
  buys is portability — exactly the right tradeoff.

## Consequences

**Positive**

- One rule for contributors: "new path → method on `Dirs`, classified by
  what it is, override precedence already implemented." Reviewers can
  reject any `dirs::home_dir().join(...)` in neenee-owned storage code on
  sight.
- Operators get a predictable, XDG-spec-compliant layout: backup
  `$XDG_DATA_HOME`; blow away `$XDG_CACHE_HOME`; snapshot
  `$XDG_CONFIG_HOME`.
- `paths::get()` lets tests inject isolated `Dirs` per test; no test
  pollutes another or the user's real state.
- The `--data-dir` / `--cache-dir` plumbing reserved in `PathsOverride`
  has a clear policy to be wired into when needed.

**Negative / neutral**

- One new method per new persistent location, on `Dirs`. The cost is a
  one-line function with a docstring naming the XDG category — low.
- macOS and Windows users see native paths (e.g.
  `~/Library/Application Support/neenee`) rather than `~/.local/share`.
  This is correct but worth documenting; the paths reference calls it
  out per platform.
- The legacy `~/.neenee/` fallback stays until a future ADR retires it.

## References

- ADR-0013 — skills-specific application of this policy (the immediate
  trigger for codifying the general rule).
- ADR-0005 — strict layering; the `Dirs` layer lives in `neenee-store`
  and is consumed upward.
- [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/latest/)
- [Paths reference](../reference/paths.md) — exact per-file lookup table.
- [Persistence explanation](../explanation/persistence.md) — the
  conceptual model behind the four-category split.
