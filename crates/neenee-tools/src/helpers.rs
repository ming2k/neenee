//! Shared helpers for built-in tools.

use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Directories that are almost never interesting to search or list and can be
/// enormous: VCS metadata, dependency trees, and build output. These are shared
/// by `grep`, `glob`, and `list_dir` so the three tools prune the *same* set
/// of directories and never disagree about what exists in a tree (previously
/// grep skipped 4 dirs, glob skipped 10, and list skipped none).
pub(crate) const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".venv",
    "venv",
    ".cache",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "coverage",
    ".gradle",
    ".idea",
    ".vscode",
];

/// True if `path` has any component matching [`IGNORED_DIRS`].
pub(crate) fn should_skip_path(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| IGNORED_DIRS.contains(&name))
    })
}

/// Extract a string field from JSON arguments for `permission_scope`.
pub(crate) fn json_string(arguments: &str, key: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| value.get(key)?.as_str().map(str::to_string))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "*".to_string())
}

/// Write `bytes` to `path` atomically: serialize to a sibling temp file,
/// `fsync` it, then `rename` over `path`.
///
/// A direct `std::fs::write` overwrites the target in place, so a crash or
/// signal mid-write leaves a **partially-written, corrupt** file. For a
/// code-editing agent that is a real data-loss vector: an interrupted
/// `edit_file` could destroy the very file it was fixing. The temp-then-rename
/// pattern keeps the previous contents intact and readable right up until the
/// (atomic) rename commits the new version.
///
/// The temp file lives next to the target (`<path>.<pid>.tmp`) so the rename
/// is on the same filesystem (POSIX requires same-filesystem for atomic
/// `rename(2)`). The temp file is best-effort removed on failure; leaving it
/// behind is harmless since the next successful write overwrites it.
pub(crate) fn save_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A unique-ish temp name: incorporating the PID avoids collisions between
    // concurrent writers without pulling in a UUID/timestamp dependency.
    let temporary = atomic_temp_path(path);
    let result = (|| -> std::io::Result<()> {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        // Best-effort: don't litter on failure. Presence is harmless (the next
        // write overwrites it) but tidy is tidy.
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// The sibling temp-file path used by [`save_file_atomic`].
fn atomic_temp_path(path: &Path) -> std::path::PathBuf {
    let pid = std::process::id();
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{pid}.tmp"));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_round_trips_and_replaces() {
        let dir = std::env::temp_dir().join(format!("neenee-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.txt");

        save_file_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");

        // A second write fully replaces the first (no append, no remnant).
        save_file_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");

        // No temp file is left behind.
        assert!(
            !dir.read_dir().unwrap().any(|e| {
                e.map(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
                    .unwrap_or(false)
            }),
            "temp file leaked"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("neenee-atomic-nested-{}", std::process::id()));
        let target = dir.join("nested/deep/file.txt");
        save_file_atomic(&target, b"hi").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hi");
        std::fs::remove_dir_all(&dir).ok();
    }
}
