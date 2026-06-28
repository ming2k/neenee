//! Dynamic catalog abstraction — the unified pattern for lists that change.
//!
//! neenee has several lists that evolve over time: provider/model catalogs
//! (from models.dev), skills (local + remote repos), MCP server tools (runtime
//! discovery), and permission rules. Hardcoding any of them means code changes
//! every time the world changes. Instead, each follows the same philosophy:
//!
//! 1. **Source of truth** — a remote API, a directory tree, a runtime protocol.
//! 2. **Local cache** — the last good copy, so a failed refresh never loses
//!    data.
//! 3. **Compiled-in fallback** — for first run / offline / corrupt cache.
//! 4. **Periodic refresh** — a background task keeps the cache current.
//! 5. **Data-driven construction** — adding an entry to the source makes it
//!    appear; no code changes in N places.
//!
//! [`DynamicCatalog`] is the thin interface every such list implements. It
//! carries only what a generic background refresh loop needs — an identifier,
//! a refresh action, and a cadence. Each implementation owns its own
//! cache/fallback/load mechanics (they differ too much across subsystems to
//! generalize), but they all speak this common refresh contract so a single
//! `spawn_refresh` in the wiring layer drives them uniformly.
//!
//! See ADR (dynamic catalog pattern) for the full rationale.

use std::time::Duration;

/// A dynamically-discoverable list that refreshes from a source of truth.
///
/// Implementations:
/// - `neenee_agent::modelsdev::ModelsDevCatalog` — providers/models from
///   models.dev (remote JSON → file cache → KNOWN_MODELS fallback).
/// - Remote skill repos — skills from HTTP repos (index.json → dir cache →
///   bundled fallback).
/// - MCP tool discovery — tools from connected servers (tools/list → in-memory
///   → last-known fallback).
///
/// The trait is intentionally minimal: `refresh` + cadence. Each implementation
/// manages its own `load` / fallback internally, because the
/// types and storage differ (JSON file vs directory tree vs subprocess state).
pub trait DynamicCatalog: Send + Sync {
    /// Stable identifier for logging and diagnostics (e.g. `"models-dev"`).
    fn id(&self) -> &'static str;

    /// Fetch the latest state from the source of truth and update the local
    /// cache. Best-effort contract: the caller logs the error and continues
    /// with the existing cache/fallback — a failed refresh must never be fatal.
    fn refresh(&self) -> impl std::future::Future<Output = Result<(), String>> + Send;

    /// How often the background loop refreshes. `Duration::ZERO` disables
    /// periodic refresh (the catalog is refreshed only at startup or on demand).
    fn refresh_period(&self) -> Duration;
}
