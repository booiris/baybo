use async_trait::async_trait;

use super::LibsqlPool;
use crate::StorageError;
use crate::secret::SecretStore;

pub struct LibsqlSecretStore {
    pool: LibsqlPool,
}

impl LibsqlSecretStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SecretStore for LibsqlSecretStore {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> crate::secret::Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "INSERT OR REPLACE INTO secrets (name, encrypted_value) VALUES (?1, ?2)",
            libsql::params![name.to_string(), encrypted_value.to_vec()],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql insert error: {e}")))?;
        Ok(())
    }

    async fn retrieve(&self, name: &str) -> crate::secret::Result<Option<Vec<u8>>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT encrypted_value FROM secrets WHERE name = ?1 AND deleted_at IS NULL",
                libsql::params![name.to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row error: {e}")))?;

        match row {
            Some(row) => {
                let value: Vec<u8> = row
                    .get(0)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    async fn list(&self) -> crate::secret::Result<Vec<String>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query("SELECT name FROM secrets WHERE deleted_at IS NULL", ())
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let mut names = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
        {
            let name: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            names.push(name);
        }
        Ok(names)
    }

    async fn delete(&self, name: &str) -> crate::secret::Result<()> {
        let now = super::time::now_us();
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE secrets SET deleted_at = ?2 \
             WHERE name = ?1 AND deleted_at IS NULL",
            libsql::params![name.to_string(), now],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete error: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_and_retrieve() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSecretStore::new(pool);
        store.store("api-key", b"encrypted-data").await.unwrap();
        let result = store.retrieve("api-key").await.unwrap();
        assert_eq!(result, Some(b"encrypted-data".to_vec()));
    }

    #[tokio::test]
    async fn retrieve_missing() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSecretStore::new(pool);
        assert!(store.retrieve("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_secrets() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSecretStore::new(pool);
        store.store("a", b"1").await.unwrap();
        store.store("b", b"2").await.unwrap();
        let mut names = store.list().await.unwrap();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }
}
