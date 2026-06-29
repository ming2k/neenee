# neenee-store

Durable state and configuration for the neenee agent stack.

`neenee-core` holds the pure domain (types & traits), zero I/O. This crate sits
one layer above it: the durable state and configuration a frontend needs to
actually run a session:

- config loading (`config.rs`) and path resolution (`paths.rs`);
- the **event-sourced session store** (which carries the pursuit primitive per
  ADR-0032), blob storage, and the embedding index;
- the per-project advisory lock (`flock`), model-usage telemetry;
- the SQLite-backed repeat/cron store (`repeat.db`).

This is the **local agent** persistence layer. It assumes a single-user
workstation: paths resolve via XDG `ProjectDirs`, sessions are keyed by project
root, and a process-level `flock` enforces single-instance-per-project.

Frontends depend on `neenee-core` + `neenee-store` and add their own
presentation layer; they never reach into a sibling frontend's crate. See
ADR-0005 for the layering rationale.
