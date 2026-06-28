//! Wiring for the dynamic-catalog pattern — the background refresh loop that
//! drives every [`DynamicCatalog`] implementation.
//!
//! Each catalog (models.dev, remote skills, MCP tools, …) implements
//! [`DynamicCatalog`]; this module provides the single [`spawn_refresh`] that
//! runs one on a schedule. The wiring layer (the CLI binary) calls it once per
//! catalog at startup, after the initial eager refresh.

use std::time::Duration;

use neenee_core::DynamicCatalog;
/// Spawn a background task that refreshes a [`DynamicCatalog`] on its declared
/// cadence. The first tick fires **immediately** (so a catalog that was not
/// refreshed eagerly at startup gets its first refresh within seconds);
/// subsequent ticks drive periodic refresh. Errors are logged and swallowed —
/// a failed refresh never kills the loop.
///
/// The task lives for the program's lifetime.
pub fn spawn_refresh(catalog: impl DynamicCatalog + 'static) {
    let id = catalog.id();
    let period = catalog.refresh_period();
    if period == Duration::ZERO {
        tracing::debug!(catalog = id, "periodic refresh disabled (period is zero)");
        return;
    }
    tokio::spawn(async move {
        // Fire an immediate first refresh so the catalog is populated without
        // blocking the startup path, then settle into the periodic cadence.
        if let Err(error) = catalog.refresh().await {
            tracing::warn!(catalog = id, %error, "initial background refresh failed");
        }
        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            if let Err(error) = catalog.refresh().await {
                tracing::warn!(catalog = id, %error, "periodic refresh failed");
            }
        }
    });
}

/// A [`DynamicCatalog`] that periodically re-scans skill sources (local dirs,
/// remote repos, bundled). Wraps a [`SkillRegistry`](crate::skills::SkillRegistry)
/// clone — the registry is `Arc<RwLock<…>>` internally, so the clone shares the
/// same live state. On refresh it calls `reload()`, which re-runs discovery
/// (including re-fetching remote repos, now with cache-as-fallback).
pub struct SkillCatalog {
    registry: crate::skills::SkillRegistry,
}

impl SkillCatalog {
    pub fn new(registry: crate::skills::SkillRegistry) -> Self {
        Self { registry }
    }
}

impl DynamicCatalog for SkillCatalog {
    fn id(&self) -> &'static str {
        "skills"
    }

    async fn refresh(&self) -> Result<(), String> {
        self.registry.reload().await;
        Ok(())
    }

    fn refresh_period(&self) -> Duration {
        Duration::from_secs(60 * 60)
    }
}
