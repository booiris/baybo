use async_trait::async_trait;
use chrono::{DateTime, Utc};

use aura_core::{AuraError, Result, Session};
use aura_session::SessionStore;

use crate::sqlite::SqlitePool;

/// SQLite-backed implementation of [`SessionStore`].
pub struct SqliteSessionStore {
    pool: SqlitePool,
}

impl SqliteSessionStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn get(&self, session_id: &str) -> Result<Option<Session>> {
        let pool = self.pool.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM sessions WHERE id = ?1")
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite prepare: {e}")))?;
            let result = stmt
                .query_row(rusqlite::params![session_id], |row| {
                    let data: String = row.get(0)?;
                    Ok(data)
                })
                .optional()
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query: {e}")))?;
            match result {
                Some(data) => {
                    let session: Session = serde_json::from_str(&data).map_err(|e| {
                        AuraError::Serialization(format!("deserialize session: {e}"))
                    })?;
                    Ok(Some(session))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn save(&self, session: &Session) -> Result<()> {
        let pool = self.pool.clone();
        let data = serde_json::to_string(session)
            .map_err(|e| AuraError::Serialization(format!("serialize session: {e}")))?;
        let id = session.id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            conn.execute(
                "INSERT OR REPLACE INTO sessions (id, data) VALUES (?1, ?2)",
                rusqlite::params![id, data],
            )
            .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite insert session: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn delete(&self, session_id: &str) -> Result<()> {
        let pool = self.pool.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            conn.execute(
                "DELETE FROM sessions WHERE id = ?1",
                rusqlite::params![session_id],
            )
            .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite delete session: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT id, data FROM sessions")
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite prepare: {e}")))?;
            let rows = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let data: String = row.get(1)?;
                    Ok((id, data))
                })
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query: {e}")))?;

            let mut expired = Vec::new();
            for row in rows {
                let (id, data) =
                    row.map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite row: {e}")))?;
                if let Ok(session) = serde_json::from_str::<Session>(&data)
                    && session.last_active < before
                {
                    expired.push(id);
                }
            }
            Ok(expired)
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
    use aura_core::{ChannelType, SessionState, User};
    use chrono::Utc;

    fn make_session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            user: User {
                id: "u1".to_string(),
                name: Some("Test".to_string()),
                channel: ChannelType::Cli,
            },
            channel: ChannelType::Cli,
            messages: vec![],
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: SessionState::default(),
        }
    }

    #[tokio::test]
    async fn test_session_crud() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteSessionStore::new(pool);

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
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteSessionStore::new(pool);

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
