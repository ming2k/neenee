//! SQLite-backed pursuit persistence.
//!
//! The pure domain types (`Pursuit`, `ThreadPursuit`, `TokenUsage`,
//! `TurnOutcome`) live in `neenee-core::pursuits`. This module holds the
//! I/O-bound layer: the `rusqlite`-backed `PursuitStore`, the `PursuitService`
//! facade over it, and the pursuit tools (`get_pursuit` / `start_pursuit` /
//! `complete_pursuit`) that query that service. ADR-0005 states `neenee-core`
//! is pure domain (zero I/O); the SQLite code therefore lives here, in the
//! persistence crate, alongside the session and blob stores.

pub mod service;
pub mod store;
pub mod tools;

pub use service::PursuitService;
pub use store::PursuitStore;
