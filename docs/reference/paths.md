# Paths

Where neenee reads and writes files. Lookup-oriented: for the conceptual
model, see [Persistence and the XDG layout](../explanation/persistence.md);
for the durable policy, see [ADR-0014](../adr/0014-xdg-persistence-architecture.md).

## Override precedence

Each XDG category resolves through the same fixed precedence, highest first.

| # | Source | Notes |
|---|--------|-------|
| 1 | CLI flag | Reserved for `--config-dir`, `--data-dir`, `--state-dir`, `--cache-dir` plumbing (not yet wired) |
| 2 | `NEENEE_CONFIG_DIR`, `NEENEE_DATA_DIR`, `NEENEE_STATE_DIR`, `NEENEE_CACHE_DIR` | App-specific env override |
| 3 | `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` | Standard XDG env override; relative values ignored per spec |
| 4 | Native per-OS default | `directories` crate: `~/.config` etc. on Linux, `~/Library/Application Support` on macOS, `%APPDATA%` on Windows |
| 5 | `$HOME/.config`, `$HOME/.local/share`, `$HOME/.local/state`, `$HOME/.cache` | Spec default when nothing else applies |
| 6 | Current working directory | Last resort; never panics |

All four categories honour the same stack — no per-subsystem special cases.

## Config — `$XDG_CONFIG_HOME/neenee/`

User-edited configuration. Lossy; back it up.

| Path | Purpose | Lossy? |
|------|---------|--------|
| `config.toml` | User-edited configuration (`[provider]`, `[skills]`, `[mcp]`, ...) | Yes |
| `.migrated-v3` | Marker that the legacy `~/.config/neenee` data files have been migrated to the split layout | Rebuildable |

Default location: `~/.config/neenee/`.

## Data — `$XDG_DATA_HOME/neenee/`

Persistent, program-generated, must survive restart. Back it up.

| Path | Purpose | Lossy? |
|------|---------|--------|
| `blobs/<2-char-prefix>/<hash>` | Content-addressed blob store for large payloads | Yes |
| `pursuits.db` | **Legacy** SQLite pursuit database (pre-ADR-0032); read once on startup to migrate into `SessionStore`, no new code writes here | Yes |
| `repeat.db` | SQLite `/repeat` cron-job database (durable recurring prompts) | Yes |
| `projects/<16-hex-bucket>/` | Per-project bucket: sessions, current pointer, metadata | Yes |
| `projects/<bucket>/embeddings.json` | Per-project lightweight embedding index | Rebuildable (re-indexed) |
| `projects/<bucket>/neenee.lock` | Per-project advisory lock | Rebuildable |
| `projects/<bucket>/permissions.json` | Per-project cached "always allow" permission rules | Rebuildable (re-prompts) |
| `sessions/` | Legacy flat session archive (transitional) | Yes |
| `skills/` | User-global skills (`SKILL.md` per skill) | Yes (user-authored) |
| `commands/` | User-global slash commands | Yes (user-authored) |

Default location: `~/.local/share/neenee/`.

The per-project bucket is `sha256(cwd)[..16]` — 16 hex chars (64 bits),
ASCII-safe, not reversible to the cwd from the path alone.

## State — `$XDG_STATE_HOME/neenee/`

Persistent, program-generated, rebuildable. Loss flattens sort order or
re-prompts; no conversation is lost.

| Path | Purpose | Lossy? |
|------|---------|--------|
| `history.json` | Slash-command input history | Rebuildable |
| `provider_usage.json` | Per-model usage telemetry driving recency sort in the provider picker | Rebuildable |
| `neenee.lock` | Cross-process advisory lock when no runtime directory is available | Rebuildable |
| `log/` | Structured rolling-log appender output (reserved) | Rebuildable |

Default location: `~/.local/state/neenee/`.

## Cache — `$XDG_CACHE_HOME/neenee/`

Derived, deletable, repopulated on demand. Safe to delete.

| Path | Purpose | Lossy? |
|------|---------|--------|
| `skills/remote/` | Cached remote skill repositories (fetched from `[skills] urls`) | Safe to delete |

Default location: `~/.cache/neenee/`.

## Runtime — `$XDG_RUNTIME_DIR/neenee/` (Linux only)

Ephemeral per-login. Honoured only when the environment provides
`XDG_RUNTIME_DIR`; otherwise neenee falls back to state. Never assume
runtime exists.

| Path | Purpose | Lossy? |
|------|---------|--------|
| `neenee.lock` | Cross-process advisory lock | Ephemeral |

## Project working tree (not under XDG)

Lives with the project root; travels with the repository.

| Path | Purpose |
|------|---------|
| `.neenee/skills/<name>/SKILL.md` | Project-local skills (highest discovery priority) |
| `.neenee/commands/<name>.md` | Project-local slash commands (highest discovery priority) |
| `.neenee/session.json`, `.neenee/sessions/` | Legacy in-project session storage ( transitional) |
| `.agents/skills/`, `.claude/skills/` | External application conventions (read-only) |
| `.agents/commands/` | External application conventions (read-only) |

The project root is located by walking upward from the current directory
looking for the first ancestor containing `.neenee`, `.git`, `Cargo.toml`,
or `package.json`.

## Bundled skills (compile-time, not on disk)

System skills are embedded into the binary at build time. They have no
on-disk location and surface as the lowest-priority discovery source. See
[ADR-0013](../adr/0013-skills-xdg-paths-and-bundled-embed.md).

## macOS and Windows defaults

The `directories` crate provides native defaults on non-Linux platforms.
The override stack is identical; only the fallback locations differ.

| Category | macOS | Windows |
|----------|-------|---------|
| Config | `~/Library/Application Support/neenee` | `%APPDATA%\neenee\config` |
| Data | `~/Library/Application Support/neenee` | `%APPDATA%\neenee\data` |
| State | `~/Library/Application Support/state` (no native state dir on macOS; falls back to the data dir's sibling `../state`) | `%LOCALAPPDATA%\neenee\state` |
| Cache | `~/Library/Caches/neenee` | `%LOCALAPPDATA%\neenee\cache` |

`XDG_*_HOME` env vars still take precedence over these on every platform.

## Cleanup quick reference

| Pursuit | Command |
|------|---------|
| Reset caches | `rm -rf $XDG_CACHE_HOME/neenee` |
| Reset rebuildable state | `rm -rf $XDG_STATE_HOME/neenee` |
| Reset one project's history | `rm -rf $XDG_DATA_HOME/neenee/projects/<bucket>` |
| Factory reset (keep config) | `rm -rf $XDG_DATA_HOME/neenee $XDG_STATE_HOME/neenee $XDG_CACHE_HOME/neenee` |
| Full wipe (including config) | Add `rm -rf $XDG_CONFIG_HOME/neenee` to the above |
