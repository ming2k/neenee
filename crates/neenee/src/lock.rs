//! Cross-process advisory lock using POSIX `flock(2)`.
//!
//! `fcntl(F_SETLK)` is the classic NFS-safe record lock, but within a single
//! process it silently replaces an existing lock, which makes same-process
//! unit testing impossible and can surprise callers that re-open the same
//! file. `flock` is per-file-description and `LOCK_NB` returns `EWOULDBLOCK`
//! even for the same process on a different fd, giving predictable exclusion
//! for the lifetime of the guard. For neenee's local lock files this is the
//! right trade-off.
//!
//! On non-Unix platforms the implementation is currently a no-op; a
//! Windows-aware implementation should use `LockFileEx` over the same file.

use std::fs::File;
use std::path::Path;

/// Guard returned after a successful lock acquisition. Dropping it closes the
/// underlying file descriptor, which releases the `flock`.
pub struct ProcessLock {
    #[cfg(unix)]
    #[allow(dead_code)]
    file: File,
    #[cfg(not(unix))]
    #[allow(dead_code)]
    _marker: (),
}

impl ProcessLock {
    /// Acquire an exclusive, non-blocking advisory lock on `path`.
    ///
    /// Returns an error if the lock cannot be obtained, most commonly because
    /// another `neenee` process already holds it for the same project.
    pub fn acquire(path: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;

            let file = File::create(path).map_err(|error| {
                format!(
                    "could not open lock file {}: {}",
                    path.display(),
                    error
                )
            })?;
            let fd = file.as_raw_fd();
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                return Err(format!(
                    "could not acquire advisory lock on {}: {} \
                     (another neenee instance may already be running for this project)",
                    path.display(), err
                ));
            }
            Ok(Self { file })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(Self { _marker: () })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_process_cannot_acquire_same_lock() {
        let dir = std::env::temp_dir().join(format!("neenee-lock-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("neenee.lock");
        std::fs::create_dir_all(&dir).unwrap();

        let first = ProcessLock::acquire(&path).expect("first acquire should succeed");
        let second = ProcessLock::acquire(&path);
        assert!(
            second.is_err(),
            "second acquire should fail while first is held"
        );

        drop(first);
        let third = ProcessLock::acquire(&path).expect("lock should be reusable after drop");
        drop(third);
        let _ = std::fs::remove_dir_all(dir);
    }
}
