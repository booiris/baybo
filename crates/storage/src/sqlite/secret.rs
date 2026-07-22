use async_trait::async_trait;
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::secret::{Result, SecretStore, StoreIdentity};

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
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()> {
        let name = name.to_string();
        let value = encrypted_value.to_vec();
        self.pool
            .interact("secrets.store", move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO secrets (name, encrypted_value) VALUES (?1, ?2)",
                    rusqlite::params![name, value],
                )?;
                Ok(())
            })
            .await
    }

    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let name = name.to_string();
        self.pool
            .interact("secrets.retrieve", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT encrypted_value FROM secrets WHERE name = ?1",
                        rusqlite::params![name],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()?)
            })
            .await
    }

    async fn list(&self) -> Result<Vec<String>> {
        self.pool
            .interact("secrets.list", move |conn| {
                let mut stmt = conn.prepare("SELECT name FROM secrets")?;
                let names = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(names)
            })
            .await
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let name = name.to_string();
        self.pool
            .interact("secrets.delete", move |conn| {
                conn.execute(
                    "DELETE FROM secrets WHERE name = ?1",
                    rusqlite::params![name],
                )?;
                Ok(())
            })
            .await
    }

    fn identity(&self) -> StoreIdentity {
        self.pool.identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_and_retrieve() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSecretStore::new(pool);
        store.store("api-key", b"encrypted-data").await.unwrap();
        let result = store.retrieve("api-key").await.unwrap();
        assert_eq!(result, Some(b"encrypted-data".to_vec()));
    }

    #[tokio::test]
    async fn retrieve_missing() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSecretStore::new(pool);
        assert!(store.retrieve("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_secrets() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSecretStore::new(pool);
        store.store("a", b"1").await.unwrap();
        store.store("b", b"2").await.unwrap();
        let mut names = store.list().await.unwrap();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }
}
