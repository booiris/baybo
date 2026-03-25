use async_trait::async_trait;
use aura_core::AuraError;
use aura_security::SecretStore;

use super::SqlitePool;

pub struct SqliteSecretStore {
    pool: SqlitePool,
}

impl SqliteSecretStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SecretStore for SqliteSecretStore {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> aura_core::Result<()> {
        let pool = self.pool.clone();
        let name = name.to_string();
        let value = encrypted_value.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            conn.execute(
                "INSERT OR REPLACE INTO secrets (name, encrypted_value) VALUES (?1, ?2)",
                rusqlite::params![name, value],
            )
            .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite insert error: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn retrieve(&self, name: &str) -> aura_core::Result<Option<Vec<u8>>> {
        let pool = self.pool.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT encrypted_value FROM secrets WHERE name = ?1")
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let result: Option<Vec<u8>> = stmt
                .query_row(rusqlite::params![name], |row| row.get(0))
                .optional()
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            Ok(result)
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn delete(&self, name: &str) -> aura_core::Result<()> {
        let pool = self.pool.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            conn.execute(
                "DELETE FROM secrets WHERE name = ?1",
                rusqlite::params![name],
            )
            .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite delete error: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn list(&self) -> aura_core::Result<Vec<String>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT name FROM secrets")
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let mut names = Vec::new();
            for row in rows {
                names.push(
                    row.map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite row error: {e}")))?,
                );
            }
            Ok(names)
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }
}

trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
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

    #[tokio::test]
    async fn store_and_retrieve() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteSecretStore::new(pool);
        store.store("api-key", b"encrypted-data").await.unwrap();
        let result = store.retrieve("api-key").await.unwrap();
        assert_eq!(result, Some(b"encrypted-data".to_vec()));
    }

    #[tokio::test]
    async fn retrieve_missing() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteSecretStore::new(pool);
        assert!(store.retrieve("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_secret() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteSecretStore::new(pool);
        store.store("key", b"val").await.unwrap();
        store.delete("key").await.unwrap();
        assert!(store.retrieve("key").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_secrets() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteSecretStore::new(pool);
        store.store("a", b"1").await.unwrap();
        store.store("b", b"2").await.unwrap();
        let mut names = store.list().await.unwrap();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }
}
