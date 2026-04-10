use thiserror::Error;
use tracing::{debug, warn};

use aura_model::{ContentBlock, MemoryCategory, MemoryEntry};
use aura_session::Session;
use aura_storage::{MemoryStore, StorageError};

#[derive(Debug, Error)]
pub enum MemoryManagerError {
    #[error("embedding error: {0}")]
    Embedding(String),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

pub type Result<T> = std::result::Result<T, MemoryManagerError>;

/// A minimal embedding model trait for generating vector embeddings from text.
#[async_trait::async_trait]
pub(crate) trait EmbeddingModel: Send + Sync {
    async fn embed(&self, text: &str) -> std::result::Result<Vec<f32>, MemoryManagerError>;
}

const DEFAULT_RECALL_LIMIT: usize = 10;
const SIMILARITY_THRESHOLD: f32 = 0.7;
const DEDUP_SIMILARITY_THRESHOLD: f32 = 0.85;

const PREFERENCE_INDICATORS: &[&str] = &[
    "i like",
    "i prefer",
    "i want",
    "please use",
    "please always",
    "don't use",
    "i dislike",
    "i hate",
    "my favorite",
];

const FACT_INDICATORS: &[&str] = &[
    "i am",
    "i work",
    "my project",
    "we use",
    "our stack",
    "my name is",
    "i live",
];

pub struct MemoryManager {
    store: Box<dyn MemoryStore>,
    embedder: Option<Box<dyn EmbeddingModel>>,
    max_entries_per_user: usize,
}

impl MemoryManager {
    pub fn without_embedder(store: Box<dyn MemoryStore>) -> Self {
        Self {
            store,
            embedder: None,
            max_entries_per_user: 1000,
        }
    }

    pub async fn recall(
        &self,
        user_id: &str,
        content: &[ContentBlock],
    ) -> Result<Vec<MemoryEntry>> {
        let text = extract_text(content);
        if text.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = if let Some(ref embedder) = self.embedder {
            self.recall_with_embedder(user_id, &text, embedder.as_ref())
                .await?
        } else {
            self.recall_without_embedder(user_id, &text).await?
        };

        results.truncate(DEFAULT_RECALL_LIMIT);

        debug!(
            user_id = user_id,
            count = results.len(),
            "recalled memories"
        );

        Ok(results)
    }

    pub async fn maybe_store(&self, session: &Session, response: &str) -> Result<()> {
        let lower = response.to_lowercase();

        for pattern in PREFERENCE_INDICATORS {
            if lower.contains(pattern) {
                let entry = MemoryEntry::new(
                    session.user.id.clone(),
                    response.to_owned(),
                    MemoryCategory::UserPreference,
                    0.7,
                );
                return self.store_with_dedup(entry).await;
            }
        }

        for pattern in FACT_INDICATORS {
            if lower.contains(pattern) {
                let entry = MemoryEntry::new(
                    session.user.id.clone(),
                    response.to_owned(),
                    MemoryCategory::KeyFact,
                    0.6,
                );
                return self.store_with_dedup(entry).await;
            }
        }

        Ok(())
    }

    pub async fn store(&self, mut entry: MemoryEntry) -> Result<()> {
        if entry.embedding.is_none()
            && let Some(ref embedder) = self.embedder
        {
            match embedder.embed(&entry.content).await {
                Ok(vec) => entry.embedding = Some(vec),
                Err(e) => {
                    warn!(error = %e, "failed to generate embedding, storing without it");
                }
            }
        }

        self.store.store(&entry).await?;
        self.enforce_user_limit(&entry.user_id).await?;
        Ok(())
    }

    async fn recall_with_embedder(
        &self,
        user_id: &str,
        text: &str,
        embedder: &dyn EmbeddingModel,
    ) -> Result<Vec<MemoryEntry>> {
        let query_vec = embedder.embed(text).await?;

        let all = self.store.list_by_user(user_id).await?;

        let mut scored: Vec<(f32, MemoryEntry)> = all
            .into_iter()
            .filter_map(|entry| {
                let sim = entry
                    .embedding
                    .as_ref()
                    .map(|emb| cosine_similarity(&query_vec, emb))
                    .unwrap_or(0.0);

                if sim >= SIMILARITY_THRESHOLD {
                    let score = sim * 0.7 + entry.importance * 0.3;
                    Some((score, entry))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        Ok(scored.into_iter().map(|(_, entry)| entry).collect())
    }

    async fn recall_without_embedder(&self, user_id: &str, text: &str) -> Result<Vec<MemoryEntry>> {
        let mut results = self
            .store
            .search(user_id, text, DEFAULT_RECALL_LIMIT)
            .await?;

        results.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    async fn store_with_dedup(&self, entry: MemoryEntry) -> Result<()> {
        let existing = self.store.list_by_user(&entry.user_id).await?;

        if let Some(ref embedder) = self.embedder {
            let query_vec = embedder.embed(&entry.content).await?;
            for existing_entry in &existing {
                if let Some(ref emb) = existing_entry.embedding {
                    let sim = cosine_similarity(&query_vec, emb);
                    if sim >= DEDUP_SIMILARITY_THRESHOLD {
                        debug!(
                            existing_id = existing_entry.id,
                            similarity = sim,
                            "skipping duplicate memory (vector)"
                        );
                        return Ok(());
                    }
                }
            }
        } else {
            for existing_entry in &existing {
                if existing_entry.content == entry.content {
                    debug!(
                        existing_id = existing_entry.id,
                        "skipping duplicate memory (text)"
                    );
                    return Ok(());
                }
            }
        }

        self.store(entry).await
    }

    async fn enforce_user_limit(&self, user_id: &str) -> Result<()> {
        let mut entries = self.store.list_by_user(user_id).await?;

        if entries.len() <= self.max_entries_per_user {
            return Ok(());
        }

        entries.sort_by(|a, b| {
            a.importance
                .partial_cmp(&b.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.last_accessed.cmp(&b.last_accessed))
        });

        let to_remove = entries.len() - self.max_entries_per_user;
        for entry in entries.iter().take(to_remove) {
            self.store.delete(&entry.id).await?;
        }

        debug!(
            user_id = user_id,
            removed = to_remove,
            "evicted memories to enforce per-user limit"
        );

        Ok(())
    }
}

fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        return 0.0;
    }

    dot / denom
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::BlobRef;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < f32::EPSILON);
    }

    #[test]
    fn test_cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn test_extract_text() {
        let blocks = vec![
            ContentBlock::Text("hello".to_string()),
            ContentBlock::Text("world".to_string()),
        ];
        assert_eq!(extract_text(&blocks), "hello world");
    }

    #[test]
    fn test_extract_text_skips_non_text() {
        let blocks = vec![
            ContentBlock::Text("hello".to_string()),
            ContentBlock::Image {
                blob: BlobRef {
                    blob_id: "x".into(),
                },
                mime_type: "image/png".into(),
            },
        ];
        assert_eq!(extract_text(&blocks), "hello");
    }

    #[test]
    fn test_memory_entry_new_clamps_importance() {
        let entry = MemoryEntry::new("u1".into(), "test".into(), MemoryCategory::KeyFact, 1.5);
        assert!((entry.importance - 1.0).abs() < f32::EPSILON);

        let entry2 = MemoryEntry::new("u1".into(), "test".into(), MemoryCategory::KeyFact, -0.5);
        assert!((entry2.importance - 0.0).abs() < f32::EPSILON);
    }
}
