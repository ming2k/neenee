# 0003. Extract `neenee-app` from the binary crate

- **Status:** Superseded by ADR-0004
- **Date:** 2026-06-20

## Context

The workspace had three crates:

- `neenee-core` — pure domain (agent loop, `Provider` trait, tool registry,
  message and goal types).
- `neenee-tui` — terminal frontend (ratatui + crossterm). Speaks to the
  backend only through `mpsc` channels of `AgentRequest` / `AgentResponse`.
- `neenee` — the binary. Owned `main.rs` **plus eleven application-service
  modules** with no TUI dependency:

  | Module | Responsibility |
  |--------|----------------|
  | `session.rs` (2328 lines) | event-sourced session persistence, compaction |
  | `events.rs` | session event log (JSON Lines) |
  | `catalog.rs` (613 lines) | model catalog → concrete `Provider` builder |
  | `paths.rs` (563 lines) | XDG path resolution |
  | `embedding.rs` | semantic-search index |
  | `config.rs` | global `config.toml` loader |
  | `model_usage.rs` | per-model usage telemetry |
  | `lock.rs` | per-project advisory `flock` |
  | `blobs.rs` / `fsutil.rs` | content-addressed store / atomic writes |
  | `search_tool.rs` | `search_history` tool wiring embeddings ↔ session |

  These formed a closed cluster (only `crate::`-references between them, all
  items `pub`) and had **no** dependency on `main.rs` or the TUI. They were
  trapped in the binary crate purely by historical accident.

The force at play: a future GUI (using the sibling `flux` immediate-mode UI
library, `../flux`) needs the exact same services — sessions, config,
catalog, embeddings, locks. A binary crate cannot be a dependency, so the
GUI would have had to either fork the code or depend on `neenee-tui` (and
inherit ratatui + crossterm), neither of which is acceptable.

A secondary cycle risk lived inside this cluster: `Config` embedded a
`neenee_tui::config::TuiConfig` field, so `config.rs` imported
`neenee-tui`. Moving `config.rs` down would have flipped the dependency
direction (`app → tui`) and made every frontend transitively pull in the
TUI.

## Decision

1. **Add a fourth crate, `neenee-app`, between `neenee-core` and the
   frontends.** Move all eleven application-service modules there. The
   dependency graph becomes acyclic and frontend-neutral:

   ```
   neenee-core   (no workspace deps)
       ↑
   neenee-app    → neenee-core
       ↑
   neenee-tui    → neenee-core, neenee-app
   neenee (bin)  → neenee-core, neenee-app, neenee-tui
   neenee-gui    → neenee-core, neenee-app, flux-ui   (future)
   ```

   No frontend depends on a sibling frontend. A GUI crate can sit alongside
   `neenee-tui` with zero new abstraction: it speaks the same
   `AgentRequest` / `AgentResponse` channel protocol the TUI already uses.

2. **Break the `Config → TuiConfig` cycle by splitting `TuiConfig` in two.**

   - The **pure-data** struct (`{ default_expanded: HashMap<String, bool> }`
     plus the `THINKING_KEY` const) moves to `neenee-app::config::TuiConfig`.
     Same serde shape, so `config.toml` keeps parsing identically. This is
     the layer every frontend reads.
   - The **presenter-aware** policy — `tool_default_expanded(name)`, which
     falls back to each tool's built-in presenter default — stays in
     `neenee-tui` because the lookup calls into
     `crate::render::tools::presenter_for`. It becomes a pair of free
     functions taking `&neenee_app::config::TuiConfig` rather than methods
     on the struct. The struct is re-exported (`pub use neenee_app::config::TuiConfig`)
     so existing TUI call sites keep compiling.

   A GUI will layer its own presenter policy on top of the same data struct,
   in its own crate.

3. **Drop `#[cfg(test)]` from two `SessionStore` helpers** (`for_path` and
   the `session_archive_dir` it calls). They were test-only constructors
   that bypass the global `paths::Dirs` table; making them `pub` lets
   external crates' tests (notably the binary's retry tests) keep pointing
   at throwaway files. The helpers remain documented as low-level
   escape hatches; normal callers still use `SessionStore::load_for_project`.

4. **Trim the binary's manifest.** `directories`, `libc`, `crc32c`, `hex`,
   `sha2` moved to `neenee-app` (they were only used by the moved modules).
   The binary keeps `tokio`, `tokio-util`, `clap`, `futures`, `toml`,
   `serde`, `serde_json`, `uuid`, `tracing`, `tracing-subscriber`,
   `tracing-appender` — all genuinely used by `main.rs`.

## Alternatives considered

- **Merge `neenee` and `neenee-tui` into one "CLI" crate.** Rejected: the
  boundary between them is already clean (the TUI takes `mpsc` ends and a
  `TuiConfig`, nothing else), and merging would force every GUI to depend
  on the TUI's render tree. The blurred boundary was below the binary, not
  above it.

- **Move the eleven modules into `neenee-core`.** Rejected: `neenee-core`
  is the pure domain (no I/O, no filesystem, no reqwest beyond providers).
  Sessions-on-disk, config files, XDG paths, and blob stores are
  application-level concerns and would muddy the core's contract.

- **Keep `TuiConfig` in `neenee-tui` and store the `[tui]` table as a raw
  `toml::Value` in `Config`.** Rejected: loses static typing in the data
  layer and pushes serde errors to the boundary. Splitting the struct is
  strictly better and required only two free functions to change shape.

- **Make `neenee-app` a new project (separate repo).** Rejected: it shares
  `Cargo.lock`, CI, and dev tooling with the rest, and depends on
  `neenee-core` via a path dep. A workspace member is the right granularity.

## Consequences

- **Positive.** A GUI crate can be added by creating `crates/neenee-gui`
  with `neenee-core` + `neenee-app` + `flux-ui` deps. No refactor of
  existing crates needed; the GUI speaks the same channel protocol as the
  TUI. The binary's `main.rs` is now genuinely thin orchestration (agent
  task spawn, slash-command dispatch, doctor subcommand) instead of also
  hosting the persistence stack.

- **Positive.** The `TuiConfig` split makes the data/policy boundary
  explicit and reproducible: data lives where it's serialized, policy lives
  where it's interpreted.

- **Neutral.** Four crates instead of three. Build time and test
  granularity both improve (each crate's tests run independently).

- **Migration was mechanical.** All items in the moved modules were already
  `pub`, and the cluster only referenced itself. The only source edits
  required were: `main.rs` imports (`mod X;` → `use neenee_app::{…}`), the
  `TuiConfig` split in two files of `neenee-tui`, and dropping the
  `#[cfg(test)]` gate on `SessionStore::for_path`. Full workspace test
  suite (354 tests) passes unchanged.

## References

- ADR-0001 — tool-step rendering redesign (motivated the `TuiConfig`
  `[tui.default_expanded]` table that this ADR relocates).
- ADR-0002 — model/channel abstraction (the catalog the binary builds via
  `neenee_app::catalog::build_provider_for`).
- `../flux/ui/README.md` — the immediate-mode UI library the future
  `neenee-gui` crate will consume via `flux-ui` Rust bindings.
