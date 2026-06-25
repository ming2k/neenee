//! Hook lifecycle registry holder, extracted from the `Agent` god-object.
//!
//! Owns the swappable [`HookRegistry`] behind a lock. The `fire_*` lifecycle
//! methods stay on [`crate::Agent`] because they depend on the agent's
//! session id (thread id) and cwd (project root); this module owns only the
//! registry storage and the cheap `Arc` snapshot used by every fire point.

use std::sync::{Arc, Mutex};

use crate::hooks::HookRegistry;

/// Internal lock-guard helper: poison-immune (recovers via `into_inner`).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds the lifecycle hook registry. The registry is swappable at runtime
/// via [`HookRunner::set`]; every read clones the `Arc` first so the lock is
/// never held across an async `fire`.
pub struct HookRunner {
    registry: Mutex<Arc<HookRegistry>>,
}

impl HookRunner {
    pub fn new() -> Self {
        Self {
            registry: Mutex::new(Arc::new(HookRegistry::empty())),
        }
    }

    /// Replace the entire registry. Intended to be called once at startup
    /// after the `[hooks]` config is parsed.
    pub fn set(&self, registry: HookRegistry) {
        *lock(&self.registry) = Arc::new(registry);
    }

    /// Snapshot the registry as a cheap `Arc` clone, so insertion points fire
    /// hooks without holding the swap lock across the async `fire`.
    pub fn get(&self) -> Arc<HookRegistry> {
        lock(&self.registry).clone()
    }
}

impl Default for HookRunner {
    fn default() -> Self {
        Self::new()
    }
}
