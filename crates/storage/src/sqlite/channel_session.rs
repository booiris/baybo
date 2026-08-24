use async_trait::async_trait;
use baybo_model::{ChannelType, SessionId};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::channel_session::{ChannelSessionStore, Result};

pub struct SqliteChannelSessionStore {
    pool: SqlitePool,
}

impl SqliteChannelSessionStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ChannelSessionStore for SqliteChannelSessionStore {
    async fn get(&self, channel_type: &ChannelType, user_id: &str) -> Result<Option<SessionId>> {
        let channel_type = channel_type.as_str().to_string();
        let user_id = user_id.to_string();
        let session_id = self
            .pool
            .interact("channel_sessions.get", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT session_id FROM channel_sessions
                 WHERE channel_type = ?1 AND user_id = ?2",
                        rusqlite::params![channel_type, user_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .await?;
        Ok(session_id.map(SessionId::from))
    }

    async fn put(
        &self,
        channel_type: &ChannelType,
        user_id: &str,
        session_id: &SessionId,
    ) -> Result<()> {
        let now = super::time::now_us();
        let channel_type = channel_type.as_str().to_string();
        let user_id = user_id.to_string();
        let session_id = session_id.as_str().to_string();
        self.pool
            .interact_write("channel_sessions.put", move |conn| {
                // Live row wins: `INSERT OR IGNORE` keeps the existing
                // `session_id` so two racing inserts don't split the mapping.
                conn.execute(
                    "INSERT OR IGNORE INTO channel_sessions (channel_type, user_id, session_id, created_at)
             VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![channel_type, user_id, session_id, now],
                )?;
                Ok(())
            })
            .await
    }

    async fn delete(&self, channel_type: &ChannelType, user_id: &str) -> Result<()> {
        let channel_type = channel_type.as_str().to_string();
        let user_id = user_id.to_string();
        self.pool
            .interact_write("channel_sessions.delete", move |conn| {
                conn.execute(
                    "DELETE FROM channel_sessions
             WHERE channel_type = ?1 AND user_id = ?2",
                    rusqlite::params![channel_type, user_id],
                )?;
                Ok(())
            })
            .await
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelSessionStore::new(pool);
        let out = store.get(&ChannelType::telegram(), "tg_42").await.unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelSessionStore::new(pool);
        store
            .put(&ChannelType::telegram(), "tg_42", &sid("sess-abc"))
            .await
            .unwrap();
        let got = store.get(&ChannelType::telegram(), "tg_42").await.unwrap();
        assert_eq!(got, Some(sid("sess-abc")));
    }

    #[tokio::test]
    async fn put_on_conflict_keeps_existing_session_id() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelSessionStore::new(pool);
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelSessionStore::new(pool);
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
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteChannelSessionStore::new(pool);
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
