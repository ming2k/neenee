//! Centralised path resolution for neenee's on-disk footprint.
//!
//! Every persistent path the program writes flows through [`Dirs`]. Resolution
//! honours the XDG Base Directory Specification and layers overrides in this
//! precedence order (highest first):
//!
//! 1. `--config-dir` / `--data-dir` / `--state-dir` / `--cache-dir` CLI flags
//!    (expressed via the matching [`PathsOverride`]).
//! 2. `NEENEE_CONFIG_DIR` / `NEENEE_DATA_DIR` / `NEENEE_STATE_DIR` /
//!    `NEENEE_CACHE_DIR` environment variables (app-specific overrides).
//! 3. `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` / `XDG_CACHE_HOME`
//!    environment variables (standard XDG overrides).
//! 4. Platform-native defaults via the `directories` crate (`config_dir`,
//!    `data_dir`, `state_dir`, `cache_dir`).
//! 5. `$HOME/.config`, `$HOME/.local/share`, ... fallbacks when even the
//!    `directories` crate cannot resolve a native location.
//!
//! On Linux `$XDG_RUNTIME_DIR` is honoured for the lock/socket/PID files; if it
//! is unset the caller is expected to fall back to the state directory.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::RwLock;

use directories::ProjectDirs;

/// App-specific override of one or more XDG roots supplied by the CLI.
///
/// Any field left as `None` falls back to env / native resolution. This is the
/// type plumbed through `main.rs` from clap.
#[derive(Debug, Clone, Default)]
pub struct PathsOverride {
    pub config_dir: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
}

/// The resolved on-disk layout. All paths are absolute and contain the `neenee`
/// segment as their final component (e.g. `~/.config/neenee`).
#[derive(Debug, Clone)]
pub struct Dirs {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    /// `$XDG_CACHE_HOME/neenee`. Currently written only lazily (remote-skill
    /// cache) and read by `remote_skills_cache` / `ensure`, which are themselves
    /// not yet wired into the production startup — kept as structural XDG state.
    #[allow(dead_code)]
    pub cache_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR/neenee` when set, otherwise `None` (callers fall back
    /// to `state_dir` for portability and to avoid surprising tmpfs use).
    pub runtime_dir: Option<PathBuf>,
}

impl Dirs {
    /// Resolve using the given CLI overrides combined with env / native.
    pub fn resolve(overrides: &PathsOverride) -> Self {
        let project = ProjectDirs::from("ai", "neenee", "neenee");
        Self {
            config_dir: resolve_kind(Kind::Config, overrides.config_dir.clone(), project.as_ref()),
            data_dir: resolve_kind(Kind::Data, overrides.data_dir.clone(), project.as_ref()),
            state_dir: resolve_kind(Kind::State, overrides.state_dir.clone(), project.as_ref()),
            cache_dir: resolve_kind(Kind::Cache, overrides.cache_dir.clone(), project.as_ref()),
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .map(|p| p.join("neenee")),
        }
    }

    /// Resolve using only env / native defaults (no CLI overrides). Convenience
    /// for code paths that have not been plumbed through `main.rs`.
    pub fn system() -> Self {
        Self::resolve(&PathsOverride::default())
    }

    // ---- well-known files --------------------------------------------------

    /// User-edited configuration. `$XDG_CONFIG_HOME/neenee/config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// Marker written once the legacy `~/.config/neenee` data files have been
    /// migrated to the XDG-split layout. Prevents re-running the migration.
    #[allow(dead_code)]
    pub fn migration_marker(&self) -> PathBuf {
        self.config_dir.join(".migrated-v2")
    }

    /// Content-addressed blob store root. Large payloads are stored under
    /// `<root>/<2-char-prefix>/<hash>`.
    pub fn blobs_dir(&self) -> PathBuf {
        self.data_dir.join("blobs")
    }

    /// Persistent, program-generated data lives under here.
    pub fn projects_dir(&self) -> PathBuf {
        self.data_dir.join("projects")
    }

    /// Per-project bucket directory: `projects/<sha256(cwd)[..16]>`. Each
    /// project's sessions, current pointer, and metadata live under their own
    /// bucket, so different working directories never see each other's
    /// sessions. The hash is truncated to 16 hex chars (64 bits) — enough to
    /// make accidental collision astronomically unlikely across a single
    /// user's projects while keeping the directory name short and ASCII-safe.
    pub fn project_dir(&self, project_root: &Path) -> PathBuf {
        self.projects_dir().join(project_bucket_name(project_root))
    }

    /// Flat session archive used by the pre-project-isolation store. Retained
    /// during the transition; new code uses [`Dirs::projects_dir`].
    pub fn legacy_sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    /// SQLite goal database, keyed by session id (thread id).
    pub fn goals_db(&self) -> PathBuf {
        self.data_dir.join("goals.db")
    }

    /// Locally installed skills (per-project skills still live under the
    /// project's working directory and are not stored here).
    #[allow(dead_code)]
    pub fn local_skills_dir(&self) -> PathBuf {
        self.data_dir.join("skills").join("local")
    }

    /// Cached remote skills (safe to delete).
    #[allow(dead_code)]
    pub fn remote_skills_cache(&self) -> PathBuf {
        self.cache_dir.join("skills").join("remote")
    }

    /// Pointer to the currently active session id per project. Rebuildable, so
    /// lives under state, not data.
    ///
    /// Reserved for Phase 2 (project isolation) where the active session id
    /// becomes a small pointer file rather than the full session content.
    #[allow(dead_code)]
    pub fn current_pointer(&self) -> PathBuf {
        self.state_dir.join("current.json")
    }

    /// Slash-command input history. Rebuildable.
    pub fn history_file(&self) -> PathBuf {
        self.state_dir.join("history.json")
    }

    /// Per-model usage telemetry (`last_used`, use count) driving recency
    /// ordering in the model picker (ADR-0002). Rebuildable: loss affects sort
    /// order only, never configuration. Sits next to [`history_file`] under
    /// `$XDG_STATE_HOME` since it is the same kind of program-generated signal.
    pub fn model_usage_file(&self) -> PathBuf {
        self.state_dir.join("model_usage.json")
    }

    /// Cross-process advisory lock file. Lives under runtime when available,
    /// otherwise state (still on a local filesystem).
    ///
    /// Phase 5 locks this file on startup with a non-blocking `flock(2)` so
    /// two `neenee` instances cannot clobber the same project's session store.
    #[allow(dead_code)]
    pub fn lock_file(&self) -> PathBuf {
        self.runtime_dir
            .clone()
            .unwrap_or_else(|| self.state_dir.clone())
            .join("neenee.lock")
    }

    /// Per-project embedding index. A lightweight brute-force index by default;
    /// future versions may swap in an HNSW/vector-DB backend using the same
    /// path convention.
    pub fn project_embeddings(&self, project_root: &Path) -> PathBuf {
        self.project_dir(project_root).join("embeddings.json")
    }

    /// Per-project advisory lock. Stored inside the project bucket so different
    /// projects can run concurrently while the same project is serialised.
    pub fn project_lock_file(&self, project_root: &Path) -> PathBuf {
        self.project_dir(project_root).join("neenee.lock")
    }

    /// Structured log directory with rolling appender output.
    #[allow(dead_code)]
    pub fn log_dir(&self) -> PathBuf {
        self.state_dir.join("log")
    }

    // ---- helpers -----------------------------------------------------------

    /// Best-effort initial creation of every directory neenee may write to.
    /// Idempotent. Errors are surfaced as a single aggregate `String`. Used by
    /// tests; production creates directories lazily via `fsutil` on first write.
    #[allow(dead_code)]
    pub fn ensure(&self) -> Result<(), String> {
        for path in [
            &self.config_dir,
            &self.data_dir,
            &self.state_dir,
            &self.cache_dir,
            &self.projects_dir(),
            &self.legacy_sessions_dir(),
            &self.local_skills_dir(),
            &self.remote_skills_cache(),
            &self.log_dir(),
        ] {
            std::fs::create_dir_all(path)
                .map_err(|e| format!("could not create directory {}: {e}", path.display()))?;
        }
        if let Some(runtime) = &self.runtime_dir {
            // Best-effort: the runtime directory is ephemeral and may not be
            // writable in sandboxes or when an unrelated test set
            // `XDG_RUNTIME_DIR`. Do not let this prevent data/state creation.
            let _ = std::fs::create_dir_all(runtime);
        }
        Ok(())
    }
}

/// Global process-wide [`Dirs`] instance. `main` installs it once via
/// [`set_default`]; every other module reads via [`Dirs::get`].
///
/// Implementation: an [`OnceLock`] holds the production value (set exactly
/// once at startup, never replaced, so production code can rely on stability).
/// A separate [`RwLock`] layered on top is used **only by tests** to swap in
/// isolated `Dirs` per test, since tests cannot reset a `OnceLock`. Production
/// reads (`Dirs::get`) check the test override first; if it is empty they fall
/// back to the `OnceLock`, then to a fresh [`Dirs::system`] resolution.
static DEFAULT: OnceLock<Dirs> = OnceLock::new();
/// Test-only override. Marked `allow(dead_code)` because the non-test build
/// compiles the static but never reads it (every accessor is `#[cfg(test)]`).
#[cfg(test)]
static TEST_OVERRIDE: RwLock<Option<Dirs>> = RwLock::new(None);

/// Install the process-wide [`Dirs`]. Idempotent: subsequent calls in the same
/// process are no-ops (the first value wins), matching production semantics.
/// Returns `Ok(None)` on first install or `Ok(Some(previous))` if a value was
/// already set (the new value is NOT stored in that case).
///
/// Not currently called in production (`get` falls back to `Dirs::system`),
/// but retained as the intended installation hook for future explicit startup.
#[allow(dead_code)]
pub fn set_default(dirs: Dirs) -> Result<Option<Dirs>, Dirs> {
    match DEFAULT.set(dirs) {
        Ok(()) => Ok(None),
        Err(existing) => Ok(Some(existing)),
    }
}

/// Test-only override of the process-wide [`Dirs`]. Pass `None` to clear.
/// Production code MUST NOT call this — it exists purely so unit tests can run
/// with isolated `data_dir`/`state_dir` roots without polluting the real
/// filesystem or racing the `OnceLock`.
#[cfg(test)]
pub fn set_test_default(dirs: Option<Dirs>) {
    *TEST_OVERRIDE.write().unwrap() = dirs;
}

/// Access the process-wide [`Dirs`]. Falls back to [`Dirs::system`] when
/// [`set_default`] has not been called yet (e.g. in tests, or in library code
/// invoked outside of `main`). When a test override is installed via
/// [`set_test_default`], that value wins over the production install.
pub fn get() -> Dirs {
    #[cfg(test)]
    if let Some(d) = TEST_OVERRIDE.read().unwrap().clone() {
        return d;
    }
    match DEFAULT.get() {
        Some(d) => d.clone(),
        None => Dirs::system(),
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    Config,
    Data,
    State,
    Cache,
}

impl Kind {
    fn app_env_var(self) -> &'static str {
        match self {
            Kind::Config => "NEENEE_CONFIG_DIR",
            Kind::Data => "NEENEE_DATA_DIR",
            Kind::State => "NEENEE_STATE_DIR",
            Kind::Cache => "NEENEE_CACHE_DIR",
        }
    }

    fn xdg_env_var(self) -> &'static str {
        match self {
            Kind::Config => "XDG_CONFIG_HOME",
            Kind::Data => "XDG_DATA_HOME",
            Kind::State => "XDG_STATE_HOME",
            Kind::Cache => "XDG_CACHE_HOME",
        }
    }

    fn fallback_segment(self) -> &'static str {
        match self {
            Kind::Config => ".config",
            Kind::Data => ".local/share",
            Kind::State => ".local/state",
            Kind::Cache => ".cache",
        }
    }

    fn native(self, project: Option<&ProjectDirs>) -> Option<PathBuf> {
        let p = project?;
        Some(match self {
            Kind::Config => p.config_dir().to_path_buf(),
            Kind::Data => p.data_dir().to_path_buf(),
            Kind::State => p
                .state_dir()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| p.data_dir().join("../state")),
            Kind::Cache => p.cache_dir().to_path_buf(),
        })
    }
}

fn resolve_kind(
    kind: Kind,
    override_path: Option<PathBuf>,
    project: Option<&ProjectDirs>,
) -> PathBuf {
    // 1. CLI flag
    if let Some(p) = override_path {
        return app_dir_from_root(p);
    }
    // 2. NEENEE_* env
    if let Some(p) = std::env::var_os(kind.app_env_var()) {
        return app_dir_from_root(PathBuf::from(p));
    }
    // 3. XDG_* env (must be absolute per spec, otherwise ignored)
    if let Some(p) = std::env::var_os(kind.xdg_env_var()) {
        let p = PathBuf::from(p);
        if p.is_absolute() {
            return app_dir_from_root(p);
        }
    }
    // 4. Native
    if let Some(p) = kind.native(project) {
        // `directories` already returns the app-suffixed path
        return p;
    }
    // 5. Home fallback
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if home.is_absolute() {
            return home.join(kind.fallback_segment()).join("neenee");
        }
    }
    // Last resort: cwd. Better than panicking.
    app_dir_from_root(PathBuf::from("."))
}

/// Given a root directory (e.g. `--data-dir=/tmp/x` or `$XDG_DATA_HOME=/foo`),
/// append the `neenee` segment unless the caller already named a directory that
/// ends in `neenee` (so `--data-dir=~/.local/share/neenee` and
/// `--data-dir=~/.local/share` both do the right thing).
fn app_dir_from_root(root: PathBuf) -> PathBuf {
    if root
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "neenee")
        .unwrap_or(false)
    {
        root
    } else {
        root.join("neenee")
    }
}

/// True when `path` is `~/.config/neenee` (the legacy single-bucket location).
/// Used by the migration logic to detect the old layout.
#[allow(dead_code)]
pub fn is_legacy_config_dir(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    parent
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "config")
        .unwrap_or(false)
}

/// Map a project root (cwd) to a stable, ASCII-safe bucket name. Uses the first
/// 16 hex chars of SHA-256 so the layout is reproducible across processes,
/// Rust versions, and platforms, and so the cwd is not leaked in the path
/// structure (paths may contain sensitive directory names).
pub fn project_bucket_name(project_root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let normalised = normalise_project_root(project_root);
    let mut hasher = Sha256::new();
    hasher.update(normalised.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// Canonicalise a project root for hashing. Redundant trailing slashes are
/// stripped, and on POSIX `..`/`.` segments are collapsed via
/// [`Path::canonicalize`] when the path actually exists; otherwise the raw path
/// is used (so a not-yet-created `--project` still produces a stable name).
fn normalise_project_root(path: &Path) -> String {
    let trimmed = path
        .to_str()
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_default();
    if trimmed.is_empty() {
        return "/".to_string();
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that mutate process-wide env vars (`XDG_*`, `NEENEE_*`, `HOME`)
    /// cannot run in parallel with each other or with tests that read those
    /// vars. We serialise them through this global lock. Tests that don't touch
    /// env vars omit the guard and can still run in parallel.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    macro_rules! env_locked {
        ($body:block) => {{
            let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
            $body
        }};
    }

    #[test]
    fn app_dir_from_root_appends_neenee_segment() {
        let p = app_dir_from_root(PathBuf::from("/tmp/foo"));
        assert_eq!(p, PathBuf::from("/tmp/foo/neenee"));
    }

    #[test]
    fn app_dir_from_root_does_not_double_append() {
        let p = app_dir_from_root(PathBuf::from("/tmp/foo/neenee"));
        assert_eq!(p, PathBuf::from("/tmp/foo/neenee"));
    }

    #[test]
    fn resolve_honours_neenee_env_over_xdg_env() {
        env_locked!({
            std::env::set_var("NEENEE_DATA_DIR", "/tmp/neenee-paths-test-data");
            std::env::set_var("XDG_DATA_HOME", "/tmp/should-not-be-used");
            let dirs = Dirs::resolve(&PathsOverride::default());
            assert_eq!(
                dirs.data_dir,
                PathBuf::from("/tmp/neenee-paths-test-data/neenee")
            );
            std::env::remove_var("NEENEE_DATA_DIR");
            std::env::remove_var("XDG_DATA_HOME");
        });
    }

    #[test]
    fn resolve_cli_override_beats_env() {
        env_locked!({
            std::env::set_var("NEENEE_DATA_DIR", "/tmp/env-loses");
            let dirs = Dirs::resolve(&PathsOverride {
                data_dir: Some(PathBuf::from("/tmp/cli-wins")),
                ..Default::default()
            });
            assert_eq!(dirs.data_dir, PathBuf::from("/tmp/cli-wins/neenee"));
            std::env::remove_var("NEENEE_DATA_DIR");
        });
    }

    #[test]
    fn resolve_ignores_relative_xdg_var() {
        env_locked!({
            std::env::set_var("XDG_CACHE_HOME", "relative/path");
            let dirs = Dirs::resolve(&PathsOverride::default());
            assert!(dirs.cache_dir.is_absolute() || dirs.cache_dir.starts_with("."));
            std::env::remove_var("XDG_CACHE_HOME");
        });
    }

    #[test]
    fn runtime_dir_only_when_xdg_runtime_dir_set() {
        env_locked!({
            std::env::remove_var("XDG_RUNTIME_DIR");
            let dirs = Dirs::resolve(&PathsOverride::default());
            assert!(dirs.runtime_dir.is_none());
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/12345");
            let dirs = Dirs::resolve(&PathsOverride::default());
            assert_eq!(
                dirs.runtime_dir.as_deref(),
                Some(std::path::Path::new("/run/user/12345/neenee"))
            );
            std::env::remove_var("XDG_RUNTIME_DIR");
        });
    }

    #[test]
    fn lock_file_prefers_runtime_dir() {
        env_locked!({
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/12345");
            let dirs = Dirs::resolve(&PathsOverride::default());
            assert_eq!(
                dirs.lock_file(),
                PathBuf::from("/run/user/12345/neenee/neenee.lock")
            );
            std::env::remove_var("XDG_RUNTIME_DIR");
        });
    }

    #[test]
    fn project_bucket_name_is_stable_and_ascii_safe() {
        let n1 = project_bucket_name(Path::new("/home/user/code/neenee"));
        let n2 = project_bucket_name(Path::new("/home/user/code/neenee"));
        assert_eq!(n1, n2, "must be stable for the same input");
        assert_eq!(n1.len(), 16, "must be 16 hex chars (8 bytes)");
        assert!(n1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn project_bucket_name_normalises_trailing_slash() {
        let a = project_bucket_name(Path::new("/foo/bar"));
        let b = project_bucket_name(Path::new("/foo/bar/"));
        assert_eq!(a, b, "trailing slash must not change the bucket");
    }

    #[test]
    fn project_bucket_name_distinguishes_different_roots() {
        let a = project_bucket_name(Path::new("/foo/aaa"));
        let b = project_bucket_name(Path::new("/foo/bbb"));
        assert_ne!(a, b);
    }

    #[test]
    fn project_dir_under_projects_root() {
        let dirs = Dirs::resolve(&PathsOverride {
            data_dir: Some(PathBuf::from("/tmp/nd")),
            ..Default::default()
        });
        let project_root = Path::new("/home/me/proj");
        let bucket = project_bucket_name(project_root);
        assert_eq!(
            dirs.project_dir(project_root),
            PathBuf::from(format!("/tmp/nd/neenee/projects/{bucket}"))
        );
    }
}
