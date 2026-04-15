use aura_model::{ChannelType, ChatMessage, Session, SessionState, User};
use aura_storage::{SessionStore, StorageError};
use chrono::{Duration, Utc};
use tracing::{debug, warn};

use crate::SessionError;

type Result<T> = std::result::Result<T, SessionError>;

fn wrap(e: StorageError) -> SessionError {
    SessionError::Storage(e.to_string())
}

/// Higher-level session management logic wrapping a `SessionStore`.
pub struct SessionManager {
    store: Box<dyn SessionStore>,
    session_timeout: Duration,
}

impl SessionManager {
    pub fn new(store: Box<dyn SessionStore>, session_timeout: Duration) -> Self {
        Self {
            store,
            session_timeout,
        }
    }

    pub async fn create_session(&self, user: User, channel: ChannelType) -> Result<Session> {
        self.create_session_with_id(uuid::Uuid::new_v4().to_string(), user, channel)
            .await
    }

    async fn create_session_with_id(
        &self,
        id: String,
        user: User,
        channel: ChannelType,
    ) -> Result<Session> {
        let now = Utc::now();
        let session = Session {
            id,
            user,
            channel,
            messages: Vec::new(),
            created_at: now,
            last_active: now,
            state: SessionState::default(),
        };
        self.store.save(&session).await.map_err(wrap)?;
        debug!(session_id = %session.id, "created new session");
        Ok(session)
    }

    pub async fn get_or_create(
        &self,
        session_id: &str,
        user: User,
        channel: ChannelType,
    ) -> Result<Session> {
        if let Some(session) = self.store.get(session_id).await.map_err(wrap)? {
            let cutoff = Utc::now() - self.session_timeout;
            if session.last_active < cutoff {
                debug!(session_id, "session expired, replacing with new session");
                self.store.delete(session_id).await.map_err(wrap)?;
                return self
                    .create_session_with_id(session_id.to_string(), user, channel)
                    .await;
            }
            debug!(session_id, "returning existing session");
            return Ok(session);
        }
        debug!(session_id, "session not found, creating new session");
        self.create_session_with_id(session_id.to_string(), user, channel)
            .await
    }

    pub async fn get(&self, session_id: &str) -> Result<Option<Session>> {
        self.store.get(session_id).await.map_err(wrap)
    }

    /// Return every session known to the underlying store, newest-active first.
    pub async fn list(&self) -> Result<Vec<Session>> {
        let mut sessions = self.store.list_all().await.map_err(wrap)?;
        sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        Ok(sessions)
    }

    /// Return the transcript (`messages`) of the given session. Errors with
    /// `SessionError::NotFound` if the session does not exist.
    pub async fn history(&self, session_id: &str) -> Result<Vec<ChatMessage>> {
        match self.store.get(session_id).await.map_err(wrap)? {
            Some(session) => Ok(session.messages),
            None => Err(SessionError::NotFound(format!("session {session_id}"))),
        }
    }

    /// Remove a session by id. Errors with `SessionError::NotFound` if the
    /// session did not exist at the time of the call so the operator sees
    /// feedback instead of a silent no-op.
    pub async fn delete(&self, session_id: &str) -> Result<()> {
        if self.store.get(session_id).await.map_err(wrap)?.is_none() {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        self.store.delete(session_id).await.map_err(wrap)?;
        debug!(session_id, "deleted session");
        Ok(())
    }

    pub async fn touch(&self, session_id: &str) -> Result<()> {
        let session = self.store.get(session_id).await.map_err(wrap)?;
        match session {
            Some(mut session) => {
                session.last_active = Utc::now();
                self.store.save(&session).await.map_err(wrap)?;
                debug!(session_id, "touched session");
                Ok(())
            }
            None => {
                warn!(session_id, "attempted to touch non-existent session");
                Err(SessionError::NotFound(format!("session {session_id}")))
            }
        }
    }

    /// Remove all sessions whose `last_active` is older than the configured timeout.
    /// Returns the number of sessions removed.
    pub async fn cleanup_expired(&self) -> Result<usize> {
        let cutoff = Utc::now() - self.session_timeout;
        let expired_ids = self.store.list_expired(cutoff).await.map_err(wrap)?;
        let count = expired_ids.len();
        for id in &expired_ids {
            self.store.delete(id).await.map_err(wrap)?;
        }
        if count > 0 {
            debug!(count, "cleaned up expired sessions");
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use aura_model::{ChannelType, Session, User};
    use aura_storage::session::Result as StoreResult;
    use chrono::{DateTime, Duration, Utc};

    use super::{SessionError, SessionManager, SessionStore};

    struct MemorySessionStore {
        data: Mutex<HashMap<String, Session>>,
    }

    impl MemorySessionStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl SessionStore for MemorySessionStore {
        async fn get(&self, session_id: &str) -> StoreResult<Option<Session>> {
            let data = self.data.lock().unwrap();
            Ok(data.get(session_id).cloned())
        }

        async fn save(&self, session: &Session) -> StoreResult<()> {
            let mut data = self.data.lock().unwrap();
            data.insert(session.id.clone(), session.clone());
            Ok(())
        }

        async fn delete(&self, session_id: &str) -> StoreResult<()> {
            let mut data = self.data.lock().unwrap();
            data.remove(session_id);
            Ok(())
        }

        async fn list_expired(&self, before: DateTime<Utc>) -> StoreResult<Vec<String>> {
            let data = self.data.lock().unwrap();
            let expired = data
                .values()
                .filter(|s| s.last_active < before)
                .map(|s| s.id.clone())
                .collect();
            Ok(expired)
        }

        async fn list_all(&self) -> StoreResult<Vec<Session>> {
            let data = self.data.lock().unwrap();
            Ok(data.values().cloned().collect())
        }
    }

    fn test_user() -> User {
        User {
            id: "user-1".to_string(),
            name: Some("Alice".to_string()),
            channel: ChannelType::Tui,
        }
    }

    #[tokio::test]
    async fn create_session_returns_valid_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::Tui)
            .await
            .unwrap();

        assert!(!session.id.is_empty());
        assert_eq!(session.user.id, "user-1");
        assert_eq!(session.channel, ChannelType::Tui);
        assert!(session.messages.is_empty());
    }

    #[tokio::test]
    async fn get_or_create_returns_existing_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::Tui)
            .await
            .unwrap();
        let session_id = session.id.clone();

        let retrieved = mgr
            .get_or_create(&session_id, test_user(), ChannelType::Tui)
            .await
            .unwrap();
        assert_eq!(retrieved.id, session_id);
    }

    #[tokio::test]
    async fn get_or_create_creates_new_when_missing() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .get_or_create("cli-abc", test_user(), ChannelType::Tui)
            .await
            .unwrap();

        assert_eq!(session.id, "cli-abc");
        let reloaded = mgr.get("cli-abc").await.unwrap();
        assert!(reloaded.is_some());
    }

    #[tokio::test]
    async fn touch_updates_last_active() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::Tui)
            .await
            .unwrap();
        let original_active = session.last_active;

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        mgr.touch(&session.id).await.unwrap();

        let updated = mgr
            .get_or_create(&session.id, test_user(), ChannelType::Tui)
            .await
            .unwrap();
        assert!(updated.last_active >= original_active);
    }

    #[tokio::test]
    async fn touch_nonexistent_returns_not_found() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let result = mgr.touch("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cleanup_expired_removes_old_sessions() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::seconds(1));

        mgr.create_session(test_user(), ChannelType::Tui)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let count = mgr.cleanup_expired().await.unwrap();
        assert_eq!(count, 1);

        let count = mgr.cleanup_expired().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn list_returns_all_sessions_newest_first() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let first = mgr
            .create_session(test_user(), ChannelType::Tui)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = mgr
            .create_session(test_user(), ChannelType::Telegram)
            .await
            .unwrap();

        let listed = mgr.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second.id);
        assert_eq!(listed[1].id, first.id);
    }

    #[tokio::test]
    async fn history_returns_messages_for_existing_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::Tui)
            .await
            .unwrap();

        let messages = mgr.history(&session.id).await.unwrap();
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn history_errors_for_missing_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let err = mgr.history("nonexistent").await.unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_removes_existing_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::Tui)
            .await
            .unwrap();

        mgr.delete(&session.id).await.unwrap();
        assert!(mgr.get(&session.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_errors_for_missing_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let err = mgr.delete("nonexistent").await.unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn get_or_create_replaces_expired_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::seconds(1));

        let session = mgr
            .create_session(test_user(), ChannelType::Tui)
            .await
            .unwrap();
        let old_id = session.id.clone();
        let old_created = session.created_at;

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let new_session = mgr
            .get_or_create(&old_id, test_user(), ChannelType::Tui)
            .await
            .unwrap();

        assert_eq!(new_session.id, old_id);
        assert!(new_session.created_at > old_created);
        assert!(new_session.messages.is_empty());
    }
}
