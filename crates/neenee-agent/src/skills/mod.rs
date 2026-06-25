//! Skills system: discover, load, and inject domain-specific expertise.
//!
//! Skills are markdown files with YAML frontmatter, stored (in priority order,
//! lowest first) across:
//!   - Bundled system skills: compile-time-embedded under
//!     `crates/neenee-agent/skills/bundled/` (see [`bundled`]).
//!   - Remote skill repositories fetched into `$XDG_CACHE_HOME/neenee/skills/remote/`.
//!   - User-global skills: `$XDG_DATA_HOME/neenee/skills/` (XDG-resolved via
//!     [`neenee_store::paths`]).
//!   - External user-global formats: `~/.agents/skills/`, `~/.claude/skills/`
//!     (someone else's convention).
//!   - Configured extra paths (`[skills] paths = [...]` in `config.toml`).
//!   - Project-local skills: `.neenee/skills/<name>/SKILL.md` (highest priority).
//!
//! Frontmatter schema:
//!   ```yaml
//!   ---
//!   name: rust-expert
//!   description: "Use when writing or debugging Rust code"
//!   short-description: "Rust help"
//!   version: "1.0.0"
//!   tags: [rust, cargo]
//!   policy:
//!     allow_implicit_invocation: true
//!   dependencies:
//!     tools:
//!       - type: mcp
//!         value: context7
//!   ---
//!   ```

pub mod bundled;
pub mod discovery;
pub mod metadata;
pub mod remote;
pub mod render;
pub mod tools;

pub use metadata::{Skill, SkillDependency, SkillPolicy, SkillScope};
pub use neenee_core::SkillsConfig;
pub use render::{build_skills_index, resolve_mentions};
pub use tools::{ListSkillsTool, ReloadSkillsTool, UseSkillTool};

use discovery::discover_all;
use std::sync::{Arc, RwLock};

/// Thread-safe in-memory registry of discovered skills.
#[derive(Clone)]
pub struct SkillRegistry {
    inner: Arc<RwLock<RegistryInner>>,
}

#[derive(Debug, Default, Clone)]
struct RegistryInner {
    skills: Vec<Skill>,
    errors: Vec<String>,
    config: SkillsConfig,
}

impl SkillRegistry {
    /// Create an empty registry.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RegistryInner::default())),
        }
    }

    /// Discover skills from all configured sources.
    pub async fn load(config: &SkillsConfig) -> Self {
        let result = discover_all(config).await;
        if !result.errors.is_empty() {
            for err in &result.errors {
                tracing::warn!("skill discovery error: {}", err);
            }
        }
        Self {
            inner: Arc::new(RwLock::new(RegistryInner {
                skills: result.skills,
                errors: result.errors,
                config: config.clone(),
            })),
        }
    }

    /// Rescan all sources using the same configuration that was originally
    /// supplied. If no configuration was stored, performs a default scan.
    pub async fn reload(&self) {
        let config = {
            match self.inner.read() {
                Ok(inner) => inner.config.clone(),
                Err(err) => err.into_inner().config.clone(),
            }
        };
        let result = discover_all(&config).await;
        if let Ok(mut inner) = self.inner.write() {
            inner.skills = result.skills;
            inner.errors = result.errors;
        }
    }

    /// Acquire a read lock on the registry.
    pub fn lock(&self) -> RegistryGuard<'_> {
        RegistryGuard {
            guard: self.inner.read().unwrap_or_else(|e| e.into_inner()),
        }
    }

    /// Replace the registry contents directly, used during tests or when the
    /// caller wants to build a registry without disk discovery.
    pub fn replace(&self, skills: Vec<Skill>) {
        if let Ok(mut inner) = self.inner.write() {
            inner.skills = skills;
            inner.errors.clear();
        }
    }
}

/// Read guard exposing registry contents.
pub struct RegistryGuard<'a> {
    guard: std::sync::RwLockReadGuard<'a, RegistryInner>,
}

impl RegistryGuard<'_> {
    pub fn get(&self, name: &str) -> Option<Skill> {
        self.guard.skills.iter().find(|s| s.name == name).cloned()
    }

    pub fn list(&self) -> Vec<Skill> {
        self.guard.skills.clone()
    }

    pub fn enabled_skills(&self) -> Vec<Skill> {
        self.guard
            .skills
            .iter()
            .filter(|s| s.enabled)
            .cloned()
            .collect()
    }

    pub fn resolve_mentions(&self, text: &str) -> Vec<Skill> {
        render::resolve_mentions(text, &self.guard.skills)
            .into_iter()
            .cloned()
            .collect()
    }
}
