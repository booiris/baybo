use async_trait::async_trait;

use super::LibsqlPool;
use crate::secret::SecretStore;
use aura_security::SecurityError;

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
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> aura_security::Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "INSERT OR REPLACE INTO secrets (name, encrypted_value) VALUES (?1, ?2)",
            libsql::params![name.to_string(), encrypted_value.to_vec()],
        )
        .await
        .map_err(|e| SecurityError::Internal(anyhow::anyhow!("libsql insert error: {e}")))?;
        Ok(())
    }

    async fn retrieve(&self, name: &str) -> aura_security::Result<Option<Vec<u8>>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT encrypted_value FROM secrets WHERE name = ?1",
                libsql::params![name.to_string()],
            )
            .await
            .map_err(|e| SecurityError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| SecurityError::Internal(anyhow::anyhow!("libsql row error: {e}")))?;

        match row {
            Some(row) => {
                let value: Vec<u8> = row
                    .get(0)
                    .map_err(|e| SecurityError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    async fn delete(&self, name: &str) -> aura_security::Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "DELETE FROM secrets WHERE name = ?1",
            libsql::params![name.to_string()],
        )
        .await
        .map_err(|e| SecurityError::Internal(anyhow::anyhow!("libsql delete error: {e}")))?;
        Ok(())
    }

    async fn list(&self) -> aura_security::Result<Vec<String>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query("SELECT name FROM secrets", ())
            .await
            .map_err(|e| SecurityError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let mut names = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| SecurityError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
        {
            let name: String = row
                .get(0)
                .map_err(|e| SecurityError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            names.push(name);
        }
        Ok(names)
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
    async fn delete_secret() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSecretStore::new(pool);
        store.store("key", b"val").await.unwrap();
        store.delete("key").await.unwrap();
        assert!(store.retrieve("key").await.unwrap().is_none());
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
