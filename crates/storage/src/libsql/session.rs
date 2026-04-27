use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::LibsqlPool;
use crate::error::StorageError;
use crate::session::{Result, SessionStore};
use aura_model::Session;

pub struct LibsqlSessionStore {
    pool: LibsqlPool,
}

impl LibsqlSessionStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SessionStore for LibsqlSessionStore {
    async fn get(&self, session_id: &str) -> Result<Option<Session>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM sessions WHERE id = ?1 AND deleted_at IS NULL",
                libsql::params![session_id.to_string()],
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
                let session: Session = serde_json::from_str(&data)
                    .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }

    async fn save(&self, session: &Session) -> Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(session)
            .map_err(|e| StorageError::Storage(format!("serialize session: {e}")))?;
        // INSERT OR REPLACE rewrites the row, so `deleted_at` falls back to
        // its NULL default — saving a previously soft-deleted id revives it.
        conn.execute(
            "INSERT OR REPLACE INTO sessions (id, data) VALUES (?1, ?2)",
            libsql::params![session.id.clone(), data],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql insert session: {e}")))?;
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<()> {
        let conn = self.pool.conn();
        let now = Utc::now().timestamp();
        conn.execute(
            "UPDATE sessions SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            libsql::params![now, session_id.to_string()],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete session: {e}")))?;
        Ok(())
    }

    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query("SELECT id, data FROM sessions WHERE deleted_at IS NULL", ())
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut expired = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let data: String = row
                .get(1)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            if let Ok(session) = serde_json::from_str::<Session>(&data)
                && session.last_active < before
            {
                expired.push(id);
            }
        }
        Ok(expired)
    }

    async fn list_all(&self) -> Result<Vec<Session>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query("SELECT data FROM sessions WHERE deleted_at IS NULL", ())
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut sessions = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let session: Session = serde_json::from_str(&data)
                .map_err(|e| StorageError::Storage(format!("deserialize session: {e}")))?;
            sessions.push(session);
        }
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, SessionState, User};

    fn make_session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            user: User {
                id: "u1".to_string(),
                name: Some("Test".to_string()),
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            trigger: Default::default(),
            parent_link: None,
            messages: vec![],
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: SessionState::default(),
        }
    }

    #[tokio::test]
    async fn test_session_crud() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let session = make_session("s1");
        store.save(&session).await.unwrap();

        let loaded = store.get("s1").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().id, "s1");

        store.delete("s1").await.unwrap();
        let loaded = store.get("s1").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_list_expired() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSessionStore::new(pool);

        let mut old_session = make_session("old");
        old_session.last_active = Utc::now() - chrono::Duration::hours(2);
        store.save(&old_session).await.unwrap();

        let new_session = make_session("new");
        store.save(&new_session).await.unwrap();

        let cutoff = Utc::now() - chrono::Duration::hours(1);
        let expired = store.list_expired(cutoff).await.unwrap();
        assert_eq!(expired, vec!["old".to_string()]);
    }
}
