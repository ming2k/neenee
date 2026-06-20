//! Semantic search over session history (C12 foundation).
//!
//! This is intentionally lightweight: an in-memory brute-force index over
//! message embeddings, persisted as JSON. A real deployment will swap the
//! [`EmbeddingProvider`] for a local model (e.g. via `ort` / `sentence-transformers`)
//! or a cloud embedding API, and replace the flat index with an HNSW/vector-DB
//! backend. The interface stays the same.

use neenee_core::{async_trait, Message};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Something that turns text into a dense vector.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, String>;
}

/// Deterministic, normalised embedding for tests and offline environments.
/// Not semantically meaningful, but stable and fast.
pub struct MockEmbeddingProvider {
    pub dims: usize,
}

impl MockEmbeddingProvider {
    pub fn new(dims: usize) -> Self {
        Self { dims }
    }
}

#[async_trait]
impl EmbeddingProvider for MockEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let digest = hasher.finalize();
        let mut vec = vec![0.0f32; self.dims];
        for (i, byte) in digest.iter().enumerate() {
            vec[i % self.dims] += *byte as f32;
        }
        // Normalise to unit length so cosine similarity is just dot product.
        let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vec {
                *v /= norm;
            }
        }
        Ok(vec)
    }
}

/// One indexed chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// Stable content hash. Used to avoid re-indexing the same text.
    content_hash: String,
    /// Human-readable source, e.g. "session_id / message 3".
    source: String,
    /// Indexed text.
    text: String,
    /// Dense embedding vector.
    embedding: Vec<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IndexFile {
    entries: Vec<Entry>,
}

/// In-memory embedding index for a single project.
pub struct EmbeddingStore {
    provider: Arc<dyn EmbeddingProvider>,
    path: PathBuf,
    index: IndexFile,
    /// Content hashes already in the index.
    seen: HashSet<String>,
}

impl EmbeddingStore {
    pub async fn open(path: PathBuf, provider: Arc<dyn EmbeddingProvider>) -> Result<Self, String> {
        let index = if path.exists() {
            let raw = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("could not read embedding index: {e}"))?;
            serde_json::from_str(&raw)
                .map_err(|e| format!("could not parse embedding index: {e}"))?
        } else {
            IndexFile::default()
        };
        let seen = index
            .entries
            .iter()
            .map(|e| e.content_hash.clone())
            .collect();
        Ok(Self {
            provider,
            path,
            index,
            seen,
        })
    }

    /// Index every message in `messages` that is not already indexed.
    pub async fn index(&mut self, messages: &[Message], session_id: &str) -> Result<(), String> {
        let mut new_entries = Vec::new();
        for (i, message) in messages.iter().enumerate() {
            self.index_message(message, session_id, i, &mut new_entries)
                .await?;
        }
        if !new_entries.is_empty() {
            self.index.entries.extend(new_entries);
            self.save().await?;
        }
        Ok(())
    }

    async fn index_message(
        &mut self,
        message: &Message,
        session_id: &str,
        index: usize,
        out: &mut Vec<Entry>,
    ) -> Result<(), String> {
        let text = message.content.trim();
        if !text.is_empty() {
            let hash = content_hash(text);
            if self.seen.insert(hash.clone()) {
                let embedding = self.provider.embed(text).await?;
                out.push(Entry {
                    content_hash: hash,
                    source: format!("{session_id} / message {index}"),
                    text: text.to_string(),
                    embedding,
                });
            }
        }
        if let Some(children) = &message.children {
            for (child_i, child) in children.iter().enumerate() {
                let source = format!("{session_id} / message {index} / child {child_i}");
                self.index_message_child(child, &source, out).await?;
            }
        }
        Ok(())
    }

    async fn index_message_child(
        &mut self,
        message: &Message,
        source: &str,
        out: &mut Vec<Entry>,
    ) -> Result<(), String> {
        let text = message.content.trim();
        if !text.is_empty() {
            let hash = content_hash(text);
            if self.seen.insert(hash.clone()) {
                let embedding = self.provider.embed(text).await?;
                out.push(Entry {
                    content_hash: hash,
                    source: source.to_string(),
                    text: text.to_string(),
                    embedding,
                });
            }
        }
        if let Some(children) = &message.children {
            for (child_i, child) in children.iter().enumerate() {
                let child_source = format!("{source} / child {child_i}");
                Box::pin(self.index_message_child(child, &child_source, out)).await?;
            }
        }
        Ok(())
    }

    /// Return the `k` most similar indexed texts for `query`.
    pub async fn search(&self, query: &str, k: usize) -> Result<Vec<(String, f32)>, String> {
        if self.index.entries.is_empty() {
            return Ok(Vec::new());
        }
        let query_vec = self.provider.embed(query).await?;
        let mut scored: Vec<(String, f32)> = self
            .index
            .entries
            .iter()
            .map(|entry| {
                let score = cosine_similarity(&query_vec, &entry.embedding);
                (format!("{}\n  {}", entry.source, entry.text), score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        scored.truncate(k);
        Ok(scored)
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }
        let raw = serde_json::to_string_pretty(&self.index).map_err(|e| e.to_string())?;
        tokio::fs::write(&self.path, raw)
            .await
            .map_err(|e| format!("could not write embedding index: {e}"))
    }
}

fn content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    for i in 0..a.len().min(b.len()) {
        dot += a[i] * b[i];
    }
    dot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_provider_embeddings_are_normalised() {
        let provider = Arc::new(MockEmbeddingProvider::new(16));
        let v = provider.embed("hello").await.unwrap();
        assert_eq!(v.len(), 16);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "vector should be unit length");
    }

    #[tokio::test]
    async fn store_indexes_and_searches() {
        let dir = std::env::temp_dir().join(format!("neenee-embedding-{}", uuid::Uuid::new_v4()));
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbeddingProvider::new(8));
        let mut store = EmbeddingStore::open(dir.join("index.json"), provider)
            .await
            .unwrap();
        let messages = vec![
            Message::new(neenee_core::Role::User, "how do I reset a password"),
            Message::new(neenee_core::Role::Assistant, "go to settings"),
        ];
        store.index(&messages, "sess-1").await.unwrap();
        let results = store.search("password reset", 1).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].0.contains("password"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
