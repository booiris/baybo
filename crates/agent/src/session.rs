use chrono::{Duration, Utc};
use tracing::{debug, warn};

use aura_session::{ChannelType, Session, SessionError, SessionState, User};
use aura_storage::SessionStore;

type Result<T> = std::result::Result<T, SessionError>;

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
        let now = Utc::now();
        let session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            user,
            channel,
            messages: Vec::new(),
            created_at: now,
            last_active: now,
            state: SessionState::default(),
        };
        self.store.save(&session).await?;
        debug!(session_id = %session.id, "created new session");
        Ok(session)
    }

    pub async fn get_or_create(
        &self,
        session_id: &str,
        user: User,
        channel: ChannelType,
    ) -> Result<Session> {
        if let Some(session) = self.store.get(session_id).await? {
            let cutoff = Utc::now() - self.session_timeout;
            if session.last_active < cutoff {
                debug!(session_id, "session expired, replacing with new session");
                self.store.delete(session_id).await?;
                return self.create_session(user, channel).await;
            }
            debug!(session_id, "returning existing session");
            return Ok(session);
        }
        debug!(session_id, "session not found, creating new session");
        self.create_session(user, channel).await
    }

    pub async fn get(&self, session_id: &str) -> Result<Option<Session>> {
        self.store.get(session_id).await
    }

    pub async fn touch(&self, session_id: &str) -> Result<()> {
        let session = self.store.get(session_id).await?;
        match session {
            Some(mut session) => {
                session.last_active = Utc::now();
                self.store.save(&session).await?;
                debug!(session_id, "touched session");
                Ok(())
            }
            None => {
                warn!(session_id, "attempted to touch non-existent session");
                Err(SessionError::NotFound(format!("session {session_id}")))
            }
        }
    }
}

#[cfg(test)]
impl SessionManager {
    async fn cleanup_expired(&self) -> Result<usize> {
        let cutoff = Utc::now() - self.session_timeout;
        let expired_ids = self.store.list_expired(cutoff).await?;
        let count = expired_ids.len();
        for id in &expired_ids {
            self.store.delete(id).await?;
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
    use chrono::{DateTime, Duration, Utc};

    use aura_session::{ChannelType, Session, User};
    use aura_storage::SessionStore;

    use super::{Result, SessionManager};

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
        async fn get(&self, session_id: &str) -> Result<Option<Session>> {
            let data = self.data.lock().unwrap();
            Ok(data.get(session_id).cloned())
        }

        async fn save(&self, session: &Session) -> Result<()> {
            let mut data = self.data.lock().unwrap();
            data.insert(session.id.clone(), session.clone());
            Ok(())
        }

        async fn delete(&self, session_id: &str) -> Result<()> {
            let mut data = self.data.lock().unwrap();
            data.remove(session_id);
            Ok(())
        }

        async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>> {
            let data = self.data.lock().unwrap();
            let expired = data
                .values()
                .filter(|s| s.last_active < before)
                .map(|s| s.id.clone())
                .collect();
            Ok(expired)
        }
    }

    fn test_user() -> User {
        User {
            id: "user-1".to_string(),
            name: Some("Alice".to_string()),
            channel: ChannelType::Cli,
        }
    }

    #[tokio::test]
    async fn create_session_returns_valid_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::Cli)
            .await
            .unwrap();

        assert!(!session.id.is_empty());
        assert_eq!(session.user.id, "user-1");
        assert_eq!(session.channel, ChannelType::Cli);
        assert!(session.messages.is_empty());
    }

    #[tokio::test]
    async fn get_or_create_returns_existing_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::Cli)
            .await
            .unwrap();
        let session_id = session.id.clone();

        let retrieved = mgr
            .get_or_create(&session_id, test_user(), ChannelType::Cli)
            .await
            .unwrap();
        assert_eq!(retrieved.id, session_id);
    }

    #[tokio::test]
    async fn get_or_create_creates_new_when_missing() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .get_or_create("nonexistent", test_user(), ChannelType::Cli)
            .await
            .unwrap();

        assert!(!session.id.is_empty());
        assert_ne!(session.id, "nonexistent");
    }

    #[tokio::test]
    async fn touch_updates_last_active() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::Cli)
            .await
            .unwrap();
        let original_active = session.last_active;

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        mgr.touch(&session.id).await.unwrap();

        let updated = mgr
            .get_or_create(&session.id, test_user(), ChannelType::Cli)
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

        mgr.create_session(test_user(), ChannelType::Cli)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let count = mgr.cleanup_expired().await.unwrap();
        assert_eq!(count, 1);

        let count = mgr.cleanup_expired().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn get_or_create_replaces_expired_session() {
        let store = Box::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::seconds(1));

        let session = mgr
            .create_session(test_user(), ChannelType::Cli)
            .await
            .unwrap();
        let old_id = session.id.clone();

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let new_session = mgr
            .get_or_create(&old_id, test_user(), ChannelType::Cli)
            .await
            .unwrap();

        assert_ne!(new_session.id, old_id);
    }
}
