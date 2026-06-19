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

use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Write `bytes` atomically: serialise to `<path>.tmp`, `fsync`, `rename` over
/// `path`, then best-effort `fsync` of `path`'s parent directory.
///
/// Returns the original [`std::io::Error`] on any failure. The temporary file
/// is best-effort cleaned up on failure (its presence is not itself corrupting —
/// the next successful write will overwrite it).
pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let result = (|| -> std::io::Result<()> {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        if let Some(parent) = path.parent() {
            if let Ok(dir) = File::open(parent) {
                // Best-effort: fsync the directory so the rename entry reaches
                // disk. Errors here (filesystems that reject syncing a dir fd)
                // are non-fatal — the data file is already durable.
                let _ = dir.sync_all();
            }
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
        assert!(!dir.join("payload.tmp").exists(), "temp file must be cleaned up");
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
