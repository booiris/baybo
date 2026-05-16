//! In-memory `MemoryStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `aura-memory` (next to the trait it
//! implements) so crates that depend on `aura-memory` but not on
//! `aura-storage` can still spin up a fake store for unit tests.

use std::collections::HashMap;

use async_trait::async_trait;
use aura_model::MemoryEntry;
use parking_lot::Mutex;

use crate::Result;
use crate::store::MemoryStore;

/// In-memory `MemoryStore` for tests. Keyed by `entry.id`. Search is a
/// case-insensitive substring match against the entry's `content` —
/// good enough for asserting "the entry I just stored shows up in
/// search".
#[derive(Debug, Default)]
pub struct MemoryMemoryStore {
    entries: Mutex<HashMap<String, MemoryEntry>>,
}

impl MemoryMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl MemoryStore for MemoryMemoryStore {
    async fn store(&self, entry: &MemoryEntry) -> Result<()> {
        self.entries.lock().insert(entry.id.clone(), entry.clone());
        Ok(())
    }

    async fn retrieve(&self, user_id: &str, key: &str) -> Result<Option<MemoryEntry>> {
        Ok(self
            .entries
            .lock()
            .values()
            .find(|e| e.user_id == user_id && e.id == key)
            .cloned())
    }

    async fn search(&self, user_id: &str, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let q = query.to_ascii_lowercase();
        Ok(self
            .entries
            .lock()
            .values()
            .filter(|e| e.user_id == user_id && e.content.to_ascii_lowercase().contains(&q))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.entries.lock().remove(id);
        Ok(())
    }

    async fn list_by_user(&self, user_id: &str) -> Result<Vec<MemoryEntry>> {
        Ok(self
            .entries
            .lock()
            .values()
            .filter(|e| e.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<MemoryEntry>> {
        Ok(self.entries.lock().values().cloned().collect())
    }

    async fn get_by_id(&self, id: &str) -> Result<Option<MemoryEntry>> {
        Ok(self.entries.lock().get(id).cloned())
    }
}
