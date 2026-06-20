//! Application-level services shared by every frontend (CLI/TUI/GUI).
//!
//! `neenee-core` holds the pure domain (agent loop, provider trait, tool
//! registry, message and goal types). This crate sits one layer above it:
//! the durable state and configuration a frontend needs to actually run a
//! session — config loading, path resolution, the event-sourced session
//! store, blob storage, the model catalog (which concrete `Provider` to
//! build from user config + env), the embedding index, the per-project
//! advisory lock, and model-usage telemetry.
//!
//! Frontends depend on `neenee-core` + `neenee-app` and add their own
//! presentation layer. They must never need to reach into a sibling
//! frontend's crate; this is what keeps the TUI reusable today and a GUI
//! reachable tomorrow (see ADR-0003).

pub mod blobs;
pub mod config;
pub mod embedding;
pub mod events;
pub mod fsutil;
pub mod lock;
pub mod model_usage;
pub mod paths;
pub mod search_tool;
pub mod session;
