//! A mutex paired with a monotonically increasing version counter.
//!
//! The render loop reads the shared transcript every frame. Deep-cloning the
//! whole `Vec<TranscriptMessage>` on every frame is O(n) in the transcript
//! length, which becomes the dominant per-frame cost in a long session — the
//! reason the TUI grows sluggish the longer it runs. `Versioned` lets the loop
//! skip the clone entirely while nothing has changed: a [`Versioned::write`]
//! guard bumps the version on drop, and the loop only re-clones when
//! [`Versioned::version`] advances past the value it last synced.
//!
//! Correctness rule: any access that mutates the inner value MUST go through
//! [`Versioned::write`]. Over-bumping (taking a `write()` guard for a read)
//! only costs an extra clone; under-bumping (mutating via [`Versioned::read`])
//! leaves the loop rendering stale state. So [`Versioned::read`] is reserved
//! for genuinely read-only access (the per-frame sync), and every mutation
//! site uses `write()`.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Mutex, MutexGuard};

/// Shared state guarded by a mutex and tagged with a version that advances on
/// every mutation, so readers can cheaply detect "nothing changed".
pub(super) struct Versioned<T> {
    inner: Mutex<T>,
    version: AtomicU64,
}

impl<T> Versioned<T> {
    /// Wrap `value`. The version starts at 1 so a loop tracking the sentinel
    /// `0` always performs its first sync.
    pub(super) fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
            version: AtomicU64::new(1),
        }
    }

    /// The current version. Lock-free; safe to poll every frame.
    pub(super) fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Acquire a read-only lock. Does **not** bump the version.
    pub(super) async fn read(&self) -> MutexGuard<'_, T> {
        self.inner.lock().await
    }

    /// Acquire a mutating lock. The version is bumped when the returned guard
    /// is dropped, so the next reader observes the change.
    pub(super) async fn write(&self) -> WriteGuard<'_, T> {
        WriteGuard {
            guard: self.inner.lock().await,
            version: &self.version,
        }
    }
}

/// A mutable guard that bumps the owning [`Versioned`]'s version on drop.
pub(super) struct WriteGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    version: &'a AtomicU64,
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        // Release so the loop's `Acquire` load in `version()` sees the bump
        // together with the mutation it is paired with.
        self.version.fetch_add(1, Ordering::Release);
    }
}
