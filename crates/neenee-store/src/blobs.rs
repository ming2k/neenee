//! Content-addressed blob store (C13 foundation).
//!
//! Large, repetitive payloads (long tool outputs, sub-agent transcripts) are
//! stored once under a SHA-256 hash and referenced by that hash. This reduces
//! duplication across sessions/forks and gives future features (semantic
//! search, sync) a stable content key.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const BLOB_PREFIX_LEN: usize = 2;

/// Store for immutable byte blobs keyed by SHA-256.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Hash bytes and return the hex digest.
    pub fn hash(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    /// Persist `bytes` and return their content hash. Idempotent: writing the
    /// same bytes twice is a no-op on disk.
    pub fn put(&self, bytes: &[u8]) -> Result<String, String> {
        let hash = Self::hash(bytes);
        let path = self.path(&hash);
        if path.exists() {
            return Ok(hash);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(&path, bytes).map_err(|e| format!("could not write blob {}: {}", hash, e))?;
        Ok(hash)
    }

    /// Read a blob by hash. Returns `None` if the blob is missing.
    pub fn get(&self, hash: &str) -> Option<Vec<u8>> {
        let path = self.path(hash);
        fs::read(&path).ok()
    }

    /// True if the blob exists locally. Test-only today; production code reads
    /// blobs directly and treats a miss as absence.
    #[cfg(test)]
    pub fn exists(&self, hash: &str) -> bool {
        self.path(hash).exists()
    }

    /// Resolve the on-disk path for a hash.
    pub fn path(&self, hash: &str) -> PathBuf {
        let prefix = &hash[..BLOB_PREFIX_LEN.min(hash.len())];
        self.root.join(prefix).join(hash)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_is_idempotent_and_get_round_trips() {
        let dir = std::env::temp_dir().join(format!("neenee-blobs-{}", uuid::Uuid::new_v4()));
        let store = BlobStore::new(dir.clone());
        let bytes = b"hello world";
        let hash1 = store.put(bytes).unwrap();
        let hash2 = store.put(bytes).unwrap();
        assert_eq!(hash1, hash2);
        assert!(store.exists(&hash1));
        assert_eq!(store.get(&hash1).unwrap(), bytes.to_vec());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn different_bytes_get_different_hashes() {
        let dir = std::env::temp_dir().join(format!("neenee-blobs-{}", uuid::Uuid::new_v4()));
        let store = BlobStore::new(dir.clone());
        let a = store.put(b"a").unwrap();
        let b = store.put(b"b").unwrap();
        assert_ne!(a, b);
        let _ = std::fs::remove_dir_all(dir);
    }
}
