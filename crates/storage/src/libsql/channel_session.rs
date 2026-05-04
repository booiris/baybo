use async_trait::async_trait;
use aura_model::ChannelType;

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
    async fn get(&self, channel_type: &ChannelType, user_id: &str) -> Result<Option<String>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT session_id FROM channel_sessions
                 WHERE channel_type = ?1 AND user_id = ?2 AND deleted_at IS NULL",
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
                Ok(Some(session_id))
            }
            None => Ok(None),
        }
    }

    async fn put(&self, channel_type: &ChannelType, user_id: &str, session_id: &str) -> Result<()> {
        let now = super::time::now_us();
        let conn = self.pool.conn();
        // Soft-delete aware upsert: clear `deleted_at` so a previously
        // tombstone mapping is revived, and keep the existing
        // `session_id` on conflict so two racing inserts don't split
        // the mapping.
        conn.execute(
            "INSERT INTO channel_sessions (channel_type, user_id, session_id, created_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, NULL)
             ON CONFLICT(channel_type, user_id) DO UPDATE SET
                 session_id = CASE
                     WHEN channel_sessions.deleted_at IS NULL THEN channel_sessions.session_id
                     ELSE excluded.session_id
                 END,
                 created_at = CASE
                     WHEN channel_sessions.deleted_at IS NULL THEN channel_sessions.created_at
                     ELSE excluded.created_at
                 END,
                 deleted_at = NULL",
            libsql::params![
                channel_type.as_str().to_string(),
                user_id.to_string(),
                session_id.to_string(),
                now,
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql upsert: {e}")))?;
        Ok(())
    }

    async fn delete(&self, channel_type: &ChannelType, user_id: &str) -> Result<()> {
        let now = super::time::now_us();
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE channel_sessions
             SET deleted_at = ?3
             WHERE channel_type = ?1 AND user_id = ?2 AND deleted_at IS NULL",
            libsql::params![channel_type.as_str().to_string(), user_id.to_string(), now,],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .put(&ChannelType::telegram(), "tg_42", "sess-abc")
            .await
            .unwrap();
        let got = store.get(&ChannelType::telegram(), "tg_42").await.unwrap();
        assert_eq!(got.as_deref(), Some("sess-abc"));
    }

    #[tokio::test]
    async fn put_on_conflict_keeps_existing_session_id() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelSessionStore::new(pool);
        store
            .put(&ChannelType::telegram(), "tg_42", "sess-first")
            .await
            .unwrap();
        store
            .put(&ChannelType::telegram(), "tg_42", "sess-second")
            .await
            .unwrap();
        let got = store.get(&ChannelType::telegram(), "tg_42").await.unwrap();
        assert_eq!(got.as_deref(), Some("sess-first"));
    }

    #[tokio::test]
    async fn delete_hides_mapping_but_put_revives() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelSessionStore::new(pool);
        store
            .put(&ChannelType::telegram(), "tg_42", "sess-a")
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

        // Revive with a fresh session id — the upsert clears
        // `deleted_at` and, since the row was tombstoned, replaces the
        // `session_id` with the newly provided one. (On a live-row
        // conflict the UPDATE is a no-op, preserving concurrent-writer
        // determinism; on a revive we want the fresh id.)
        store
            .put(&ChannelType::telegram(), "tg_42", "sess-b")
            .await
            .unwrap();
        let got = store.get(&ChannelType::telegram(), "tg_42").await.unwrap();
        assert_eq!(got.as_deref(), Some("sess-b"));
    }

    #[tokio::test]
    async fn different_channel_types_do_not_collide() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlChannelSessionStore::new(pool);
        store
            .put(&ChannelType::telegram(), "tg_42", "sess-tg")
            .await
            .unwrap();
        store
            .put(&ChannelType::discord(), "tg_42", "sess-dc")
            .await
            .unwrap();
        assert_eq!(
            store
                .get(&ChannelType::telegram(), "tg_42")
                .await
                .unwrap()
                .as_deref(),
            Some("sess-tg"),
        );
        assert_eq!(
            store
                .get(&ChannelType::discord(), "tg_42")
                .await
                .unwrap()
                .as_deref(),
            Some("sess-dc"),
        );
    }
}
