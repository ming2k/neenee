//! Filesystem durability helpers shared by [`crate::config`] and
//! [`crate::session`].
//!
//! The functions here implement the **atomic-rename + fsync** durability
//! pattern required for crash-safe single-file updates. POSIX guarantees that
//! `rename(2)` is atomic on the same filesystem, but only `fsync(2)` forces the
//! data and metadata to durable media. Without an additional `fsync` of the
//! parent directory, ext4 in particular can reorder the directory entry update
//! such that a power loss after `rename` leaves neither the old nor the new
//! file reachable.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Owner-only mode (`rw-------`) applied to every file we write and `rwx------`
/// to its parent directory on Unix. Config and session files hold secrets (API
/// keys) and private conversation content, so they must never be group- or
/// world-readable regardless of the caller's umask.
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;

/// Create the leaf parent directory of `path` (and any missing ancestors),
/// then best-effort tighten the leaf to owner-only on Unix.
fn create_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Best-effort: an already-existing dir keeps its mode; we only
            // tighten, never loosen, and a failure here is non-fatal.
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(DIR_MODE));
        }
    }
    Ok(())
}

/// Create `path` for writing with owner-only permissions from the moment it
/// exists, so there is never a window where the file is group/world-readable.
fn create_private_file(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        File::create(path)
    }
}

/// Write `bytes` atomically: serialise to `<path>.tmp`, `fsync`, `rename` over
/// `path`, then best-effort `fsync` of `path`'s parent directory.
///
/// On Unix the temp file is created `rw-------` and its parent directory
/// tightened to `rwx------`, so secrets (API keys, conversation history) never
/// land on disk group- or world-readable.
///
/// Returns the original [`std::io::Error`] on any failure. The temporary file
/// is best-effort cleaned up on failure (its presence is not itself corrupting —
/// the next successful write will overwrite it).
pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    create_parent_dir(path)?;
    let temporary = path.with_extension("tmp");
    let result = (|| -> std::io::Result<()> {
        let mut file = create_private_file(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        if let Some(parent) = path.parent()
            && let Ok(dir) = File::open(parent)
        {
            // Best-effort: fsync the directory so the rename entry reaches
            // disk. Errors here (filesystems that reject syncing a dir fd)
            // are non-fatal — the data file is already durable.
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Atomically write a pretty-printed JSON value. Convenience wrapper around
/// [`atomic_write_bytes`]. `?Sized` so it accepts slices like `&[String]`.
pub fn atomic_write_json<T: serde::Serialize + ?Sized>(
    path: &Path,
    value: &T,
) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    atomic_write_bytes(path, &bytes).map_err(|e| e.to_string())
}

/// Guard for a blocking exclusive advisory lock held on a companion
/// `<path>.lock` file. Used to serialise short read-modify-write windows on
/// shared global files (`provider_usage.json`, slash-command history, the
/// per-project embedding index) so two concurrently-running `neenee` instances
/// in the same project — or across projects — never silently lose each other's
/// updates. Dropping the guard closes the fd and releases the lock.
///
/// The lock is held on a **companion** file rather than the data file itself
/// because the data file is rewritten via temp-file + `rename(2)` (see
/// [`atomic_write_bytes`]), which swaps the underlying inode. A `flock` on the
/// data file's old fd would not protect the newly-renamed inode, so concurrent
/// writers could each believe they held the lock. The companion file is
/// opened once and never renamed, so its file-description lock reliably
/// serialises every holder for the lock's lifetime.
///
/// On non-Unix platforms this is a structural no-op (the companion file is
/// created for symmetry, but no mutual exclusion is enforced). The state it
/// guards is rebuildable cosmetic telemetry, so the residual race is
/// acceptable there and is documented in ADR-0018.
pub struct FileLock {
    #[cfg(unix)]
    _file: File,
    #[cfg(not(unix))]
    _file: (),
}

impl FileLock {
    /// Acquire a blocking exclusive lock on `<path>.lock`. Creates the
    /// companion file (and its parent directory) if missing, then blocks
    /// until an exclusive `flock(2)` is obtained.
    pub fn acquire(path: &Path) -> std::io::Result<Self> {
        let lock_path = lock_companion(path);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&lock_path)?;
            // Blocking exclusive lock. We intentionally do not pass `LOCK_NB`:
            // RMW sections are short, so waiting for the prior holder is both
            // correct (no lost update) and cheap.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { _file: file })
        }
        #[cfg(not(unix))]
        {
            let _ = OpenOptions::new()
                .create(true)
                .write(true)
                .open(&lock_path)?;
            Ok(Self { _file: () })
        }
    }
}

/// Companion lock-file path for `path`: `<path>.lock`.
fn lock_companion(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Sample {
        name: &'static str,
        n: u32,
    }

    #[test]
    fn atomic_write_round_trips_and_removes_tmp() {
        let dir = std::env::temp_dir().join(format!("neenee-fsutil-{}", uuid::Uuid::new_v4()));
        let path = dir.join("payload.json");
        atomic_write_json(&path, &Sample { name: "ok", n: 7 }).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"name\": \"ok\""));
        assert!(text.contains("\"n\": 7"));
        assert!(
            !dir.join("payload.tmp").exists(),
            "temp file must be cleaned up"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("neenee-fsutil-{}-perm", uuid::Uuid::new_v4()));
        let path = dir.join("secret.json");
        atomic_write_json(&path, &Sample { name: "k", n: 1 }).unwrap();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "secret file must be rw-------");
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "secret dir must be rwx------");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = std::env::temp_dir().join(format!("neenee-fsutil-{}-2", uuid::Uuid::new_v4()));
        let path = dir.join("payload.json");
        atomic_write_json(&path, &Sample { name: "v1", n: 1 }).unwrap();
        atomic_write_json(&path, &Sample { name: "v2", n: 2 }).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"name\": \"v2\""));
        assert!(!text.contains("v1"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
