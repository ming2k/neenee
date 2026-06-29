//! Durable state and configuration for the neenee agent stack.
//!
//! `neenee-core` holds the pure domain (types & traits), zero I/O. This
//! crate sits one layer above it: the durable state and configuration a
//! frontend needs to actually run a session — config loading, path
//! resolution, the event-sourced session store (which carries the pursuit
//! primitive per ADR-0032), blob storage, the embedding index, the
//! per-project advisory lock, model-usage telemetry, and the SQLite-backed
//! repeat-cron (`repeat.db`) store. The repeat store lived in `neenee-core`
//! before the ADR-0005 "zero-I/O core" boundary was enforced; it moved here
//! so core stays free of `rusqlite`. The shared migration helpers (`db`)
//! moved with it.
//!
//! This is the **local agent** persistence layer. It assumes a
//! single-user workstation: paths resolve via XDG `ProjectDirs`, sessions
//! are keyed by project root, and a process-level `flock` enforces
//! single-instance-per-project. Other scenarios the project may grow
//! (group-chat with multi-tenancy, always-on quant trading) will not fit
//! this layer and should spawn sibling crates (`neenee-chat-store`,
//! `neenee-trading-store`) sharing only `neenee-core`. See ADR-0005.
//!
//! Frontends depend on `neenee-core` + `neenee-store` and add their own
//! presentation layer. They must never need to reach into a sibling
//! frontend's crate; this is what keeps the CLI self-contained today and
//! a GUI reachable tomorrow.

pub mod blobs;
pub mod cache;
pub mod config;
pub mod db;
pub mod embedding;
pub mod events;
pub mod fsutil;
pub mod legacy_pursuit;
pub mod lock;
pub mod paths;
pub mod provider_usage;
pub mod repeat;
pub mod search_tool;
pub mod session;

pub use repeat::RepeatStore;
