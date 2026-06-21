//! Durable state and configuration for the coding-agent stack.
//!
//! `neenee-core` holds the pure domain (types & traits). This crate sits
//! one layer above it: the durable state and configuration a frontend
//! needs to actually run a session — config loading, path resolution,
//! the event-sourced session store, blob storage, the embedding index,
//! the per-project advisory lock, and model-usage telemetry.
//!
//! This is the **local coding-agent** persistence layer. It assumes a
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
pub mod config;
pub mod embedding;
pub mod events;
pub mod fsutil;
pub mod lock;
pub mod paths;
pub mod provider_usage;
pub mod search_tool;
pub mod session;
