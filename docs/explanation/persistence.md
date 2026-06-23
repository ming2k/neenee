# Persistence and the XDG layout

neenee writes a lot to disk: conversations, file blobs, embeddings,
telemetry, advisory locks, cached skills. The question "where does this
file live, what am I allowed to do with it, and what happens if I delete
it" must have one answer per file — and that answer must be derivable from
the file's *nature*, not looked up in a table every time.

This page is the conceptual model. For the per-file lookup, see
[Paths reference](../reference/paths.md). For the durable decision record,
see [ADR-0014](../adr/0014-xdg-persistence-architecture.md).

## Why XDG

The [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/latest/)
splits user-level files into categories that have **different operational
lifetimes**. That split is what makes backup, snapshot, and cleanup
tractable. neenee adopts it wholesale rather than inventing its own
`~/.neenee/` bucket — and rather than each subsystem inventing its own
answer to "where do I put this".

The historical alternative — one monolithic `~/.neenee/` directory — had
three problems in practice:

1. **Backup blur.** Configuration, conversations, and rebuildable caches
   sat together. Backing up "neenee" meant either too much (caches) or
   too little (only config).
2. **Cleanup ambiguity.** Nothing was safe to delete without reading the
   code to find out whether it would regenerate.
3. **Override impossibility.** Containerised, sandboxed, and multi-prefix
   setups had no knob short of `HOME=`, which is far too coarse.

## The four categories

neenee classifies every file by **what it is**, then routes it to the
matching XDG category.

### Config — files the user edits by hand

`$XDG_CONFIG_HOME/neenee/` (default `~/.config/neenee/`).

The only file here today is `config.toml`, the hand-edited configuration.
Losing it is lossy: it captures user preferences and provider setup.
Restoring from backup is the right move.

### Data — persistent, program-generated, must survive restart

`$XDG_DATA_HOME/neenee/` (default `~/.local/share/neenee/`).

Conversations, content-addressed blobs, the pursuit database, per-project
embedding indices, cached permission approvals, and user-authored skills
and commands. This is the irreplaceable history of the work the user has
done. Back it up.

The per-project bucket (under `projects/<short-hash>/`) keeps each
working directory's history isolated — different projects never see each
other's sessions. The hash is short (16 hex chars, 64 bits) to keep
names readable while keeping accidental collision across a single user's
projects astronomically unlikely.

### State — persistent, program-generated, rebuildable

`$XDG_STATE_HOME/neenee/` (default `~/.local/state/neenee/`).

Slash-command history, per-model usage telemetry that orders the provider
picker by recency, advisory lock files when no runtime directory is
available. Loss is non-fatal: it flattens sort order or forces a
re-prompt, but no conversation or skill is lost.

### Cache — derived, deletable, repopulated on demand

`$XDG_CACHE_HOME/neenee/` (default `~/.cache/neenee/`).

The remote-skill cache. Safe to delete at any time; the next startup
that needs a remote skill fetches it again. Treat as ephemeral.

### Runtime (Linux only) — ephemeral per login

`$XDG_RUNTIME_DIR/neenee/` when the variable is set.

Cross-process advisory locks. neenee honours this only when the
environment provides it; on platforms without `XDG_RUNTIME_DIR`, locks
fall back to state. Never assume runtime exists.

## What is *not* under XDG

Two categories of file deliberately live outside XDG:

- **The project working tree.** Project-local skills (`.neenee/skills/`),
  project-local commands (`.neenee/commands/`), and plan-mode plan files
  (`.neenee/plans/`) live with the project. They travel with the
  repository and are owned by the project, not the user's environment.
- **External applications' conventions.** neenee *reads* skills from
  `~/.agents/skills/`, `~/.claude/skills/`
  because those are other tools' locations. neenee never writes to them.

The bundled system skills are not on disk at all — they are
compile-time-embedded into the binary. See
[ADR-0013](../adr/0013-skills-xdg-paths-and-bundled-embed.md).

## Override precedence

XDG categories answer *where*. The override stack answers *who decides*.
From highest to lowest:

1. **CLI flag.** Reserved for `--config-dir`, `--data-dir`,
   `--state-dir`, `--cache-dir` plumbing. The type exists; flag wiring
   is reserved for a future change.
2. **App-specific env var.** `NEENEE_CONFIG_DIR`, `NEENEE_DATA_DIR`,
   `NEENEE_STATE_DIR`, `NEENEE_CACHE_DIR`. Use these to redirect
   neenee and only neenee.
3. **Standard XDG env var.** `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
   `XDG_STATE_HOME`, `XDG_CACHE_HOME`. Affects every compliant
   application; ideal for shared setups.
4. **Native per-OS default.** On macOS, `~/Library/Application
   Support/neenee`; on Windows, `%APPDATA%\neenee`. Provided by the
   platform's convention rather than the spec.
5. **`$HOME` fallback.** `~/.config`, `~/.local/share`, `~/.local/state`,
   `~/.cache` — the spec's default locations when nothing else applies.
6. **Current directory.** Last resort; never panics.

The same precedence applies to every category — there is no per-subsystem
special case. Relative values in the XDG env vars are ignored (per spec);
absolute values win.

## What is safe to delete

| Delete | Consequence |
|--------|-------------|
| `$XDG_CACHE_HOME/neenee/` | None. Cache regenerates. |
| `$XDG_STATE_HOME/neenee/` | Recency-based sort orders reset; permission caches drop and re-prompt on next session. |
| `$XDG_DATA_HOME/neenee/projects/<bucket>/` | That project loses its session history and embeddings. |
| `$XDG_DATA_HOME/neenee/` | All history, blobs, skills, commands, pursuits. Effectively a factory reset; `config.toml` survives. |
| `$XDG_CONFIG_HOME/neenee/` | Loses user-edited configuration. Sessions and skills survive. |
