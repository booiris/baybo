use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use aura_core::Result;
use aura_memory::{MemoryEntry, MemoryStore};

/// In-memory implementation of [`MemoryStore`].
///
/// The `search` method performs a naive substring match on `MemoryEntry.content`
/// since there is no embedding index in the in-memory backend.
pub struct InMemoryMemoryStore {
    /// Key: entry id
    data: Arc<RwLock<HashMap<String, MemoryEntry>>>,
}

impl InMemoryMemoryStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MemoryStore for InMemoryMemoryStore {
    async fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let mut guard = self.data.write().await;
        guard.insert(entry.id.clone(), entry.clone());
        Ok(())
    }

    async fn retrieve(&self, user_id: &str, key: &str) -> Result<Option<MemoryEntry>> {
        let guard = self.data.read().await;
        let found = guard
            .values()
            .find(|e| e.user_id == user_id && e.id == key)
            .cloned();
        Ok(found)
    }

    async fn search(&self, user_id: &str, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let guard = self.data.read().await;
        let query_lower = query.to_lowercase();
        let mut results: Vec<MemoryEntry> = guard
            .values()
            .filter(|e| e.user_id == user_id && e.content.to_lowercase().contains(&query_lower))
            .cloned()
            .collect();
        // Sort by importance descending so higher-importance entries come first.
        results.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let mut guard = self.data.write().await;
        guard.remove(id);
        Ok(())
    }

    async fn list_by_user(&self, user_id: &str) -> Result<Vec<MemoryEntry>> {
        let guard = self.data.read().await;
        let entries = guard
            .values()
            .filter(|e| e.user_id == user_id)
            .cloned()
            .collect();
        Ok(entries)
    }
}
