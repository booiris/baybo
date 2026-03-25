use chrono::Utc;
use tracing::{debug, warn};

use aura_core::{ContentBlock, Result, Session};

use crate::{EmbeddingModel, MemoryCategory, MemoryEntry, MemoryStore};

// ---------------------------------------------------------------------------
// Configuration constants
// ---------------------------------------------------------------------------

const DEFAULT_RECALL_LIMIT: usize = 10;
const SIMILARITY_THRESHOLD: f32 = 0.7;
const DEDUP_SIMILARITY_THRESHOLD: f32 = 0.85;

/// Keyword-based patterns that hint at user preferences.
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

/// Keyword-based patterns that hint at key facts.
const FACT_INDICATORS: &[&str] = &[
    "i am",
    "i work",
    "my project",
    "we use",
    "our stack",
    "my name is",
    "i live",
];

// ---------------------------------------------------------------------------
// MemoryManager
// ---------------------------------------------------------------------------

pub struct MemoryManager {
    store: Box<dyn MemoryStore>,
    embedder: Option<Box<dyn EmbeddingModel>>,
    max_entries_per_user: usize,
}

impl MemoryManager {
    pub fn new(store: Box<dyn MemoryStore>, embedder: Box<dyn EmbeddingModel>) -> Self {
        Self {
            store,
            embedder: Some(embedder),
            max_entries_per_user: 1000,
        }
    }

    pub fn without_embedder(store: Box<dyn MemoryStore>) -> Self {
        Self {
            store,
            embedder: None,
            max_entries_per_user: 1000,
        }
    }

    /// Set the maximum number of memory entries allowed per user.
    pub fn with_max_entries_per_user(mut self, max: usize) -> Self {
        self.max_entries_per_user = max;
        self
    }

    // -------------------------------------------------------------------
    // Public API
    // -------------------------------------------------------------------

    /// Recall relevant memories for the given user based on the current
    /// conversation content. When an embedder is available, vector cosine
    /// similarity is used; otherwise falls back to keyword search.
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

        // Cap the result count.
        results.truncate(DEFAULT_RECALL_LIMIT);

        debug!(
            user_id = user_id,
            count = results.len(),
            "recalled memories"
        );

        Ok(results)
    }

    /// Analyse the session response and automatically store a memory if the
    /// content is deemed worth remembering.
    pub async fn maybe_store(&self, session: &Session, response: &str) -> Result<()> {
        let lower = response.to_lowercase();

        // Check preference indicators.
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

        // Check fact indicators.
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

    /// Persist a memory entry, generating an embedding if an embedder is
    /// configured and the entry does not already carry one.
    pub async fn store(&self, mut entry: MemoryEntry) -> Result<()> {
        // Generate embedding when missing.
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

    /// Remove all expired memory entries across all users. Returns the count
    /// of entries removed.
    pub async fn forget_expired(&self) -> Result<usize> {
        // We cannot push expiration logic into a single store call without a
        // richer trait method, so we iterate known users. In practice, the
        // storage layer should implement a bulk delete; for now we do
        // client-side filtering to keep the trait simple.
        //
        // A future optimization would add a `delete_expired` method to
        // `MemoryStore`.

        // There is no `list_all_users` on the store, so we rely on the
        // caller to scope this per-user if needed. As a practical
        // middle-ground we return 0 and log a warning when no user scope is
        // available.  The more common path is `forget_expired_for_user`.
        warn!("forget_expired without user scope is a no-op; use forget_expired_for_user");
        Ok(0)
    }

    /// Remove expired memory entries for a specific user. Returns the count
    /// of entries removed.
    pub async fn forget_expired_for_user(&self, user_id: &str) -> Result<usize> {
        let entries = self.store.list_by_user(user_id).await?;
        let now = Utc::now();
        let mut removed = 0usize;

        for entry in &entries {
            if let Some(expires_at) = entry.expires_at
                && expires_at < now
            {
                self.store.delete(&entry.id).await?;
                removed += 1;
            }
        }

        if removed > 0 {
            debug!(
                user_id = user_id,
                removed = removed,
                "expired memories removed"
            );
        }

        Ok(removed)
    }

    // -------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------

    /// Recall using vector cosine similarity.
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
                    // Blend similarity (70 %) with importance (30 %).
                    let score = sim * 0.7 + entry.importance * 0.3;
                    Some((score, entry))
                } else {
                    None
                }
            })
            .collect();

        // Sort descending by blended score.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        Ok(scored.into_iter().map(|(_, entry)| entry).collect())
    }

    /// Recall using keyword / text search provided by the store.
    async fn recall_without_embedder(&self, user_id: &str, text: &str) -> Result<Vec<MemoryEntry>> {
        let mut results = self
            .store
            .search(user_id, text, DEFAULT_RECALL_LIMIT)
            .await?;

        // Sort by importance descending.
        results.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    /// Store an entry after checking for duplicates. If a semantically
    /// similar entry already exists, update it instead of inserting a new one.
    async fn store_with_dedup(&self, entry: MemoryEntry) -> Result<()> {
        let existing = self.store.list_by_user(&entry.user_id).await?;

        if let Some(ref embedder) = self.embedder {
            // Vector-based dedup.
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
            // Simple text dedup — exact content match.
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

    /// Enforce `max_entries_per_user` by evicting the least valuable entries.
    async fn enforce_user_limit(&self, user_id: &str) -> Result<()> {
        let mut entries = self.store.list_by_user(user_id).await?;

        if entries.len() <= self.max_entries_per_user {
            return Ok(());
        }

        // Sort by importance ascending, then by last_accessed ascending, so
        // the least valuable entries come first.
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

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Extract all text from a slice of `ContentBlock`s.
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

/// Compute cosine similarity between two vectors. Returns `0.0` when either
/// vector has zero magnitude or the vectors differ in length.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        use aura_core::BlobRef;
        let blocks = vec![
            ContentBlock::Text("hello".to_string()),
            ContentBlock::Image {
                blob: BlobRef {
                    blob_id: "x".into(),
                    size_bytes: 0,
                    sha256: "x".into(),
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
