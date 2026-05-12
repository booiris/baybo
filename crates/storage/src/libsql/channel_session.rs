use async_trait::async_trait;
use aura_model::{ChannelType, SessionId};

use super::LibsqlPool;
use crate::StorageError;
use crate::channel_session::{ChannelSessionStore, Result};

pub struct LibsqlChannelSessionStore {
    pool: LibsqlPool,
}

impl LibsqlChannelSessionStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ChannelSessionStore for LibsqlChannelSessionStore {
    async fn get(&self, channel_type: &ChannelType, user_id: &str) -> Result<Option<SessionId>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT session_id FROM channel_sessions
                 WHERE channel_type = ?1 AND user_id = ?2",
                libsql::params![channel_type.as_str().to_string(), user_id.to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query: {e}")))?;
        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row: {e}")))?;
        match row {
            Some(row) => {
                let session_id: String = row
                    .get(0)
                    .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                Ok(Some(SessionId::from(session_id)))
            }
            None => Ok(None),
        }
    }

    async fn put(
        &self,
        channel_type: &ChannelType,
        user_id: &str,
        session_id: &SessionId,
    ) -> Result<()> {
        let now = super::time::now_us();
        let conn = self.pool.conn();
        // Live row wins: `INSERT OR IGNORE` keeps the existing
        // `session_id` so two racing inserts don't split the mapping.
        conn.execute(
            "INSERT OR IGNORE INTO channel_sessions (channel_type, user_id, session_id, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            libsql::params![
                channel_type.as_str().to_string(),
                user_id.to_string(),
                session_id.as_str().to_string(),
                now,
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql upsert: {e}")))?;
        Ok(())
    }

    async fn delete(&self, channel_type: &ChannelType, user_id: &str) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "DELETE FROM channel_sessions
             WHERE channel_type = ?1 AND user_id = ?2",
            libsql::params![channel_type.as_str().to_string(), user_id.to_string()],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId::from(s)
    }

    #[tokio::test]
    async fn get_returns_none_for_missing_mapping() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelSessionStore::new(pool);
        let out = store.get(&ChannelType::telegram(), "tg_42").await.unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelSessionStore::new(pool);
        store
            .put(&ChannelType::telegram(), "tg_42", &sid("sess-abc"))
            .await
            .unwrap();
        let got = store.get(&ChannelType::telegram(), "tg_42").await.unwrap();
        assert_eq!(got, Some(sid("sess-abc")));
    }

    #[tokio::test]
    async fn put_on_conflict_keeps_existing_session_id() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelSessionStore::new(pool);
        store
            .put(&ChannelType::telegram(), "tg_42", &sid("sess-first"))
            .await
            .unwrap();
        store
            .put(&ChannelType::telegram(), "tg_42", &sid("sess-second"))
            .await
            .unwrap();
        let got = store.get(&ChannelType::telegram(), "tg_42").await.unwrap();
        assert_eq!(got, Some(sid("sess-first")));
    }

    #[tokio::test]
    async fn delete_then_put_creates_fresh_mapping() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelSessionStore::new(pool);
        store
            .put(&ChannelType::telegram(), "tg_42", &sid("sess-a"))
            .await
            .unwrap();
        store
            .delete(&ChannelType::telegram(), "tg_42")
            .await
            .unwrap();
        assert!(
            store
                .get(&ChannelType::telegram(), "tg_42")
                .await
                .unwrap()
                .is_none()
        );

        // After the row is gone, a fresh put inserts the new session id.
        // (On a live-row conflict `INSERT OR IGNORE` keeps the existing
        // row, preserving concurrent-writer determinism.)
        store
            .put(&ChannelType::telegram(), "tg_42", &sid("sess-b"))
            .await
            .unwrap();
        let got = store.get(&ChannelType::telegram(), "tg_42").await.unwrap();
        assert_eq!(got, Some(sid("sess-b")));
    }

    #[tokio::test]
    async fn different_channel_types_do_not_collide() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelSessionStore::new(pool);
        store
            .put(&ChannelType::telegram(), "tg_42", &sid("sess-tg"))
            .await
            .unwrap();
        store
            .put(&ChannelType::discord(), "tg_42", &sid("sess-dc"))
            .await
            .unwrap();
        assert_eq!(
            store.get(&ChannelType::telegram(), "tg_42").await.unwrap(),
            Some(sid("sess-tg")),
        );
        assert_eq!(
            store.get(&ChannelType::discord(), "tg_42").await.unwrap(),
            Some(sid("sess-dc")),
        );
    }
}
