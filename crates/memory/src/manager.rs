use tracing::debug;

use aura_model::MemoryEntry;

use crate::MemoryError;
use crate::Result;
use crate::store::MemoryStore;

const DEFAULT_MAX_ENTRIES_PER_USER: usize = 1000;

pub struct MemoryManager {
    store: std::sync::Arc<dyn MemoryStore>,
    max_entries_per_user: usize,
}

impl MemoryManager {
    pub fn new(store: std::sync::Arc<dyn MemoryStore>) -> Self {
        Self {
            store,
            max_entries_per_user: DEFAULT_MAX_ENTRIES_PER_USER,
        }
    }

    pub async fn store(&self, entry: MemoryEntry) -> Result<()> {
        self.store.store(&entry).await?;
        self.enforce_user_limit(&entry.user_id).await?;
        Ok(())
    }

    /// List stored memories. When `user_id` is `Some`, results are scoped to
    /// that user; when `None`, returns every entry across all users
    /// (operator view used by `memory list`).
    pub async fn list(&self, user_id: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let entries = match user_id {
            Some(u) => self.store.list_by_user(u).await?,
            None => self.store.list_all().await?,
        };
        Ok(entries)
    }

    /// Substring-search memories by content. When `user_id` is `Some`, the
    /// store's index-aware search is used; otherwise a scan of all entries is
    /// performed in-process (operator mode).
    pub async fn search(
        &self,
        user_id: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        match user_id {
            Some(u) => Ok(self.store.search(u, query, limit).await?),
            None => {
                let needle = query.to_lowercase();
                let mut entries = self.store.list_all().await?;
                entries.retain(|e| e.content.to_lowercase().contains(&needle));
                entries.truncate(limit);
                Ok(entries)
            }
        }
    }

    /// Look a memory entry up by its stable id, without a user scope.
    pub async fn get(&self, id: &str) -> Result<Option<MemoryEntry>> {
        self.store.get_by_id(id).await
    }

    /// Delete a single memory entry by id. Returns `Ok(())` even if the entry
    /// did not exist — idempotent.
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.store.delete(id).await?;
        Ok(())
    }

    /// Clamp an entry's importance to `[0.0, 1.0]` and persist. Returns the
    /// updated entry for audit purposes (`memory promote` echoes the new
    /// value). Errors with `NotFound` if the id is unknown.
    pub async fn set_importance(&self, id: &str, importance: f32) -> Result<MemoryEntry> {
        let mut entry = self
            .store
            .get_by_id(id)
            .await?
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
        entry.importance = importance.clamp(0.0, 1.0);
        self.store.store(&entry).await?;
        Ok(entry)
    }

    /// Delete every memory entry whose `source_session_id` matches. Returns
    /// the number of entries removed — the CLI's `memory clear --session`
    /// surfaces this count so the operator can confirm the blast radius.
    pub async fn delete_for_session(&self, session_id: &str) -> Result<u64> {
        let entries = self.store.list_all().await?;
        let mut count = 0u64;
        for entry in entries {
            if entry.source_session_id.as_deref() == Some(session_id) {
                self.store.delete(&entry.id).await?;
                count += 1;
            }
        }
        Ok(count)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryMemoryStore;
    use aura_model::MemoryCategory;

    fn make_entry(user: &str, content: &str, session: Option<&str>) -> MemoryEntry {
        let mut e = MemoryEntry::new(user.into(), content.into(), MemoryCategory::KeyFact, 0.5);
        e.source_session_id = session.map(str::to_string);
        e
    }

    #[test]
    fn test_memory_entry_new_clamps_importance() {
        let entry = MemoryEntry::new("u1".into(), "test".into(), MemoryCategory::KeyFact, 1.5);
        assert!((entry.importance - 1.0).abs() < f32::EPSILON);

        let entry2 = MemoryEntry::new("u1".into(), "test".into(), MemoryCategory::KeyFact, -0.5);
        assert!((entry2.importance - 0.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn list_returns_user_subset_when_scoped() {
        let store = std::sync::Arc::new(MemoryMemoryStore::new());
        let mgr = MemoryManager::new(store);
        mgr.store(make_entry("u1", "a", None)).await.unwrap();
        mgr.store(make_entry("u2", "b", None)).await.unwrap();
        mgr.store(make_entry("u1", "c", None)).await.unwrap();

        let scoped = mgr.list(Some("u1")).await.unwrap();
        assert_eq!(scoped.len(), 2);
        assert!(scoped.iter().all(|e| e.user_id == "u1"));
    }

    #[tokio::test]
    async fn list_returns_every_entry_when_unscoped() {
        let store = std::sync::Arc::new(MemoryMemoryStore::new());
        let mgr = MemoryManager::new(store);
        mgr.store(make_entry("u1", "a", None)).await.unwrap();
        mgr.store(make_entry("u2", "b", None)).await.unwrap();

        let all = mgr.list(None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn search_global_matches_across_users() {
        let store = std::sync::Arc::new(MemoryMemoryStore::new());
        let mgr = MemoryManager::new(store);
        mgr.store(make_entry("u1", "Rust rocks", None))
            .await
            .unwrap();
        mgr.store(make_entry("u2", "rusty bolts", None))
            .await
            .unwrap();
        mgr.store(make_entry("u2", "Python only", None))
            .await
            .unwrap();

        let hits = mgr.search(None, "rust", 10).await.unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn set_importance_clamps_and_persists() {
        let store = std::sync::Arc::new(MemoryMemoryStore::new());
        let mgr = MemoryManager::new(store);
        let entry = make_entry("u1", "anchor", None);
        let id = entry.id.clone();
        mgr.store(entry).await.unwrap();

        let out = mgr.set_importance(&id, 2.0).await.unwrap();
        assert!((out.importance - 1.0).abs() < f32::EPSILON);
        let reloaded = mgr.get(&id).await.unwrap().unwrap();
        assert!((reloaded.importance - 1.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn set_importance_errors_when_missing() {
        let store = std::sync::Arc::new(MemoryMemoryStore::new());
        let mgr = MemoryManager::new(store);
        let err = mgr.set_importance("nope", 0.5).await.unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_for_session_removes_only_matching_entries() {
        let store = std::sync::Arc::new(MemoryMemoryStore::new());
        let mgr = MemoryManager::new(store);
        mgr.store(make_entry("u1", "from s1", Some("s1")))
            .await
            .unwrap();
        mgr.store(make_entry("u1", "also s1", Some("s1")))
            .await
            .unwrap();
        mgr.store(make_entry("u1", "unrelated", Some("s2")))
            .await
            .unwrap();
        mgr.store(make_entry("u1", "orphan", None)).await.unwrap();

        let removed = mgr.delete_for_session("s1").await.unwrap();
        assert_eq!(removed, 2);
        let remaining = mgr.list(None).await.unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(
            remaining
                .iter()
                .all(|e| e.source_session_id.as_deref() != Some("s1"))
        );
    }
}
