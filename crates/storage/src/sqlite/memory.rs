use async_trait::async_trait;

use aura_core::{AuraError, Result};
use aura_memory::{MemoryEntry, MemoryStore};

use crate::sqlite::SqlitePool;

/// SQLite-backed implementation of [`MemoryStore`].
pub struct SqliteMemoryStore {
    pool: SqlitePool,
}

impl SqliteMemoryStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MemoryStore for SqliteMemoryStore {
    async fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let pool = self.pool.clone();
        let id = entry.id.clone();
        let user_id = entry.user_id.clone();
        let content = entry.content.clone();
        let data = serde_json::to_string(entry)
            .map_err(|e| AuraError::Serialization(format!("serialize memory entry: {e}")))?;
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            conn.execute(
                "INSERT OR REPLACE INTO memories (id, user_id, content, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, user_id, content, data],
            )
            .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite insert memory: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn retrieve(&self, user_id: &str, key: &str) -> Result<Option<MemoryEntry>> {
        let pool = self.pool.clone();
        let user_id = user_id.to_string();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM memories WHERE id = ?1 AND user_id = ?2")
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite prepare: {e}")))?;
            let result = stmt
                .query_row(rusqlite::params![key, user_id], |row| {
                    let data: String = row.get(0)?;
                    Ok(data)
                })
                .optional()
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query: {e}")))?;
            match result {
                Some(data) => {
                    let entry: MemoryEntry = serde_json::from_str(&data).map_err(|e| {
                        AuraError::Serialization(format!("deserialize memory entry: {e}"))
                    })?;
                    Ok(Some(entry))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn search(&self, user_id: &str, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let pool = self.pool.clone();
        let user_id = user_id.to_string();
        let pattern = format!("%{}%", query.to_lowercase());
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT data FROM memories WHERE user_id = ?1 AND LOWER(content) LIKE ?2 LIMIT ?3",
                )
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite prepare: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![user_id, pattern, limit as i64], |row| {
                    let data: String = row.get(0)?;
                    Ok(data)
                })
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query: {e}")))?;
            let mut entries = Vec::new();
            for row in rows {
                let data =
                    row.map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite row: {e}")))?;
                let entry: MemoryEntry = serde_json::from_str(&data).map_err(|e| {
                    AuraError::Serialization(format!("deserialize memory entry: {e}"))
                })?;
                entries.push(entry);
            }
            Ok(entries)
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let pool = self.pool.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            conn.execute("DELETE FROM memories WHERE id = ?1", rusqlite::params![id])
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite delete memory: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn list_by_user(&self, user_id: &str) -> Result<Vec<MemoryEntry>> {
        let pool = self.pool.clone();
        let user_id = user_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM memories WHERE user_id = ?1")
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite prepare: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![user_id], |row| {
                    let data: String = row.get(0)?;
                    Ok(data)
                })
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query: {e}")))?;
            let mut entries = Vec::new();
            for row in rows {
                let data =
                    row.map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite row: {e}")))?;
                let entry: MemoryEntry = serde_json::from_str(&data).map_err(|e| {
                    AuraError::Serialization(format!("deserialize memory entry: {e}"))
                })?;
                entries.push(entry);
            }
            Ok(entries)
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }
}

/// Helper to convert `rusqlite::Error::QueryReturnedNoRows` into `None`.
trait OptionalExt<T> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for std::result::Result<T, rusqlite::Error> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_memory::MemoryCategory;
    use chrono::Utc;

    fn make_entry(id: &str, user_id: &str, content: &str) -> MemoryEntry {
        let now = Utc::now();
        MemoryEntry {
            id: id.to_string(),
            user_id: user_id.to_string(),
            content: content.to_string(),
            category: MemoryCategory::KeyFact,
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
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteMemoryStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteMemoryStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteMemoryStore::new(pool);

        store.store(&make_entry("m1", "u1", "a")).await.unwrap();
        store.store(&make_entry("m2", "u1", "b")).await.unwrap();
        store.store(&make_entry("m3", "u2", "c")).await.unwrap();

        let entries = store.list_by_user("u1").await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn test_memory_delete() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteMemoryStore::new(pool);

        store.store(&make_entry("m1", "u1", "a")).await.unwrap();
        store.delete("m1").await.unwrap();

        let loaded = store.retrieve("u1", "m1").await.unwrap();
        assert!(loaded.is_none());
    }
}
