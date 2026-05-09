use async_trait::async_trait;

use super::LibsqlPool;
use crate::error::StorageError;
use crate::memory::{MemoryStore, Result};
use aura_model::MemoryEntry;

pub struct LibsqlMemoryStore {
    pool: LibsqlPool,
}

impl LibsqlMemoryStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MemoryStore for LibsqlMemoryStore {
    async fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(entry)
            .map_err(|e| StorageError::Storage(format!("serialize memory entry: {e}")))?;
        conn.execute(
            "INSERT OR REPLACE INTO memories (id, user_id, content, data) VALUES (?1, ?2, ?3, ?4)",
            libsql::params![
                entry.id.clone(),
                entry.user_id.clone(),
                entry.content.clone(),
                data,
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql insert memory: {e}")))?;
        Ok(())
    }

    async fn retrieve(&self, user_id: &str, key: &str) -> Result<Option<MemoryEntry>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM memories WHERE id = ?1 AND user_id = ?2",
                libsql::params![key.to_string(), user_id.to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?;

        match row {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let entry: MemoryEntry = serde_json::from_str(&data)
                    .map_err(|e| StorageError::Storage(format!("deserialize memory entry: {e}")))?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    async fn search(&self, user_id: &str, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let conn = self.pool.conn();
        let pattern = format!("%{}%", query.to_lowercase());
        let mut rows = conn
            .query(
                "SELECT data FROM memories \
                 WHERE user_id = ?1 AND LOWER(content) LIKE ?2 \
                 LIMIT ?3",
                libsql::params![user_id.to_string(), pattern, limit as i64],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut entries = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let entry: MemoryEntry = serde_json::from_str(&data)
                .map_err(|e| StorageError::Storage(format!("deserialize memory entry: {e}")))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "DELETE FROM memories WHERE id = ?1",
            libsql::params![id.to_string()],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete memory: {e}")))?;
        Ok(())
    }

    async fn list_by_user(&self, user_id: &str) -> Result<Vec<MemoryEntry>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM memories WHERE user_id = ?1",
                libsql::params![user_id.to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut entries = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let entry: MemoryEntry = serde_json::from_str(&data)
                .map_err(|e| StorageError::Storage(format!("deserialize memory entry: {e}")))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    async fn list_all(&self) -> Result<Vec<MemoryEntry>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query("SELECT data FROM memories", ())
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut entries = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let entry: MemoryEntry = serde_json::from_str(&data)
                .map_err(|e| StorageError::Storage(format!("deserialize memory entry: {e}")))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    async fn get_by_id(&self, id: &str) -> Result<Option<MemoryEntry>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM memories WHERE id = ?1",
                libsql::params![id.to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?;

        match row {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let entry: MemoryEntry = serde_json::from_str(&data)
                    .map_err(|e| StorageError::Storage(format!("deserialize memory entry: {e}")))?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::MemoryCategory;
    use chrono::Utc;

    fn make_entry(id: &str, user_id: &str, content: &str) -> MemoryEntry {
        let now = Utc::now();
        MemoryEntry {
            id: id.to_string(),
            user_id: user_id.to_string(),
            content: content.to_string(),
            category: MemoryCategory::User,
            importance: 0.5,
            embedding: None,
            created_at: now,
            last_accessed: now,
            source_session_id: None,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn test_memory_store_and_retrieve() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlMemoryStore::new(pool);

        let entry = make_entry("m1", "u1", "hello world");
        store.store(&entry).await.unwrap();

        let loaded = store.retrieve("u1", "m1").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(
            loaded.as_ref().map(|e| e.content.as_str()),
            Some("hello world")
        );
    }

    #[tokio::test]
    async fn test_memory_search() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlMemoryStore::new(pool);

        store
            .store(&make_entry("m1", "u1", "Rust programming"))
            .await
            .unwrap();
        store
            .store(&make_entry("m2", "u1", "Python scripting"))
            .await
            .unwrap();
        store
            .store(&make_entry("m3", "u2", "Rust macros"))
            .await
            .unwrap();

        let results = store.search("u1", "rust", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "m1");
    }

    #[tokio::test]
    async fn test_memory_list_by_user() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlMemoryStore::new(pool);

        store.store(&make_entry("m1", "u1", "a")).await.unwrap();
        store.store(&make_entry("m2", "u1", "b")).await.unwrap();
        store.store(&make_entry("m3", "u2", "c")).await.unwrap();

        let entries = store.list_by_user("u1").await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn test_memory_delete() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlMemoryStore::new(pool);

        store.store(&make_entry("m1", "u1", "a")).await.unwrap();
        store.delete("m1").await.unwrap();

        let loaded = store.retrieve("u1", "m1").await.unwrap();
        assert!(loaded.is_none());
    }
}
