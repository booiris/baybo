use std::sync::Arc;

use aura_model::{ChannelType, ChatMessage, Session, SessionId, SessionState, TriggerSource, User};
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
    store: Arc<dyn SessionStore>,
    session_timeout: Duration,
    /// Default soul version stamped on new sessions when the caller
    /// does not supply one. The agent layer overrides this via
    /// `create_session_with_options` once it loads the live config.
    default_soul_version: String,
}

impl SessionManager {
    pub fn new(store: Arc<dyn SessionStore>, session_timeout: Duration) -> Self {
        Self {
            store,
            session_timeout,
            default_soul_version: "soul-default".to_owned(),
        }
    }

    /// Override the soul version stamped on subsequently-created
    /// sessions. Useful in tests and at startup once the soul
    /// configuration is known.
    pub fn with_default_soul_version(mut self, soul_version: impl Into<String>) -> Self {
        self.default_soul_version = soul_version.into();
        self
    }

    /// Read-only view of the underlying `SessionStore`. Used by
    /// callers (CLI / gateway admin) that need to construct
    /// `QueryApi` against the same store the manager writes to.
    pub fn store(&self) -> Arc<dyn SessionStore> {
        Arc::clone(&self.store)
    }

    pub async fn create_session(&self, user: User, channel: ChannelType) -> Result<Session> {
        self.create_session_with_trigger(user, channel, TriggerSource::User)
            .await
    }

    /// Mint a brand-new session with an explicit `TriggerSource`. Used
    /// by the cron router so each fire gets an isolated session
    /// (`TriggerSource::Cron { cron_job_id }` stamped at creation,
    /// continuity across fires deferred to long-term memory rather
    /// than a shared mutable transcript). The id is prefixed by trigger
    /// kind (`cron-` / `system-`) so logs and admin listings can
    /// recognize machine-triggered sessions at a glance; `User`
    /// sessions stay bare-UUID.
    pub async fn create_session_with_trigger(
        &self,
        user: User,
        channel: ChannelType,
        trigger: TriggerSource,
    ) -> Result<Session> {
        let prefix = match &trigger {
            TriggerSource::User => "",
            TriggerSource::Cron { .. } => "cron-",
            TriggerSource::System { .. } => "system-",
        };
        let id = SessionId::from(format!("{prefix}{}", uuid::Uuid::new_v4()));
        self.create_session_with_id(id, user, channel, trigger)
            .await
    }

    /// Create a session that descends from `parent` via the given
    /// lineage (subagent or user-fork). The child inherits its
    /// trigger from the parent's root session and gets a fresh
    /// session_id prefixed with `subagent-` / `fork-` as a hint.
    pub async fn create_spawned_session(
        &self,
        user: User,
        channel: ChannelType,
        parent: &Session,
        lineage: aura_model::Lineage,
    ) -> Result<Session> {
        let prefix = match lineage.kind {
            aura_model::LineageKind::Subagent => "subagent-",
            aura_model::LineageKind::UserFork { .. } => "fork-",
        };
        let id = SessionId::from(format!("{prefix}{}", uuid::Uuid::new_v4()));
        let now = Utc::now();
        let session = Session {
            id: id.clone(),
            user,
            channel,
            created_at: now,
            last_active: now,
            state: aura_model::SessionState::default(),
            // Spawned sessions inherit `root_session_id` from the
            // ultimate ancestor, not from their direct parent.
            root_session_id: parent.root_session_id.clone(),
            // Trigger inherits from root (Q2 design).
            trigger: parent.trigger.clone(),
            lineage: Some(lineage),
            bound_soul_version: self.default_soul_version.clone(),
        };
        self.store.save(&session).await.map_err(wrap)?;
        debug!(
            session_id = %session.id,
            parent_session_id = %parent.id,
            "spawned subagent / fork session"
        );
        Ok(session)
    }

    async fn create_session_with_id(
        &self,
        id: SessionId,
        user: User,
        channel: ChannelType,
        trigger: TriggerSource,
    ) -> Result<Session> {
        let now = Utc::now();
        let session = Session {
            id: id.clone(),
            user,
            channel,
            created_at: now,
            last_active: now,
            state: SessionState::default(),
            root_session_id: id,
            trigger,
            lineage: None,
            bound_soul_version: self.default_soul_version.clone(),
        };
        self.store.save(&session).await.map_err(wrap)?;
        debug!(session_id = %session.id, "created new session");
        Ok(session)
    }

    pub async fn get_or_create(
        &self,
        session_id: &SessionId,
        user: User,
        channel: ChannelType,
    ) -> Result<Session> {
        if let Some(session) = self.store.get(session_id).await.map_err(wrap)? {
            let cutoff = Utc::now() - self.session_timeout;
            if session.last_active < cutoff {
                debug!(session_id = %session_id, "session expired, replacing with new session");
                self.store.delete(session_id).await.map_err(wrap)?;
                return self
                    .create_session_with_id(session_id.clone(), user, channel, TriggerSource::User)
                    .await;
            }
            debug!(session_id = %session_id, "returning existing session");
            return Ok(session);
        }
        debug!(session_id = %session_id, "session not found, creating new session");
        self.create_session_with_id(session_id.clone(), user, channel, TriggerSource::User)
            .await
    }

    pub async fn get(&self, session_id: &SessionId) -> Result<Option<Session>> {
        self.store.get(session_id).await.map_err(wrap)
    }

    /// Return every session known to the underlying store, newest-active first.
    pub async fn list(&self) -> Result<Vec<Session>> {
        let mut sessions = self.store.list_all().await.map_err(wrap)?;
        sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        Ok(sessions)
    }

    /// Return the active transcript for the session — non-superseded
    /// rows of the append-only `session_messages` log, in order.
    /// Errors with `SessionError::NotFound` if the session itself
    /// does not exist; an existing session with no turns yet returns
    /// an empty vector.
    pub async fn history(&self, session_id: &SessionId) -> Result<Vec<ChatMessage>> {
        if self.store.get(session_id).await.map_err(wrap)?.is_none() {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        self.store
            .load_active_session_messages(session_id)
            .await
            .map_err(wrap)
    }

    /// Append a single message to the session's transcript log.
    /// Called by `AgentLoop` from the `&mut self` append paths so
    /// every turn's worth of messages reaches storage incrementally
    /// rather than via a per-turn full-blob rewrite.
    pub async fn append_session_message(
        &self,
        session_id: &SessionId,
        message: &ChatMessage,
    ) -> Result<()> {
        self.store
            .append_session_message(session_id, message)
            .await
            .map_err(wrap)
    }

    /// Mark the currently-active rows for `session_id` as superseded
    /// by the next ordinal and append `new_active` at the next
    /// contiguous ordinals. Used by `AgentLoop::compact_now` /
    /// `compress_if_needed` after a successful compression apply.
    pub async fn apply_session_compaction(
        &self,
        session_id: &SessionId,
        new_active: &[ChatMessage],
    ) -> Result<()> {
        self.store
            .apply_session_compaction(session_id, new_active)
            .await
            .map_err(wrap)
    }

    /// Load the active transcript for `session_id` — used by the
    /// router to seed `ContextManager` on actor cold start. Returns
    /// an empty vector when no turns have been recorded yet.
    pub async fn load_active_session_messages(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ChatMessage>> {
        self.store
            .load_active_session_messages(session_id)
            .await
            .map_err(wrap)
    }

    /// Hard-delete a session by id. Errors with `SessionError::NotFound`
    /// if the session did not exist at the time of the call. Surfaces
    /// `StorageError::HasLiveForks` (wrapped) when the session has live
    /// forks pointing at it.
    pub async fn delete(&self, session_id: &SessionId) -> Result<()> {
        let deleted = self.store.delete(session_id).await.map_err(wrap)?;
        if !deleted {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        debug!(session_id = %session_id, "deleted session");
        Ok(())
    }

    pub async fn touch(&self, session_id: &SessionId) -> Result<()> {
        let session = self.store.get(session_id).await.map_err(wrap)?;
        match session {
            Some(mut session) => {
                session.last_active = Utc::now();
                self.store.save(&session).await.map_err(wrap)?;
                debug!(session_id = %session_id, "touched session");
                Ok(())
            }
            None => {
                warn!(session_id = %session_id, "attempted to touch non-existent session");
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
        let deletes = expired_ids.iter().map(|id| self.store.delete(id));
        futures::future::try_join_all(deletes).await.map_err(wrap)?;
        if count > 0 {
            debug!(count, "cleaned up expired sessions");
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use aura_model::{ChannelType, ChatMessage, Session, SessionId, User};
    use aura_storage::session::Result as StoreResult;
    use chrono::{DateTime, Duration, Utc};
    use parking_lot::Mutex;

    use super::{SessionError, SessionManager, SessionStore};

    /// One stored row in the test fake — mirrors the libsql layout
    /// closely enough that `apply_session_compaction` can supersede
    /// rows the same way the real backend does.
    #[derive(Clone)]
    struct StoredMessage {
        ordinal: u64,
        message: ChatMessage,
        superseded_by: Option<u64>,
    }

    struct MemorySessionStore {
        data: Mutex<HashMap<SessionId, Session>>,
        transcripts: Mutex<HashMap<SessionId, Vec<StoredMessage>>>,
    }

    impl MemorySessionStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
                transcripts: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl SessionStore for MemorySessionStore {
        async fn get(&self, session_id: &SessionId) -> StoreResult<Option<Session>> {
            Ok(self.data.lock().get(session_id).cloned())
        }

        async fn save(&self, session: &Session) -> StoreResult<()> {
            self.data.lock().insert(session.id.clone(), session.clone());
            Ok(())
        }

        async fn delete(&self, session_id: &SessionId) -> StoreResult<bool> {
            self.transcripts.lock().remove(session_id);
            Ok(self.data.lock().remove(session_id).is_some())
        }

        async fn list_expired(&self, before: DateTime<Utc>) -> StoreResult<Vec<SessionId>> {
            Ok(self
                .data
                .lock()
                .values()
                .filter(|s| s.last_active < before)
                .map(|s| s.id.clone())
                .collect())
        }

        async fn list_all(&self) -> StoreResult<Vec<Session>> {
            Ok(self.data.lock().values().cloned().collect())
        }

        async fn list_live_forks(
            &self,
            _source_session_id: &SessionId,
        ) -> StoreResult<Vec<SessionId>> {
            // Test fake — we never construct lineage children here.
            Ok(Vec::new())
        }

        async fn list_lineage_children(
            &self,
            _parent_session_id: &SessionId,
        ) -> StoreResult<Vec<(SessionId, aura_model::LineageKind)>> {
            Ok(Vec::new())
        }

        async fn append_session_message(
            &self,
            session_id: &SessionId,
            message: &ChatMessage,
        ) -> StoreResult<()> {
            let mut guard = self.transcripts.lock();
            let log = guard.entry(session_id.clone()).or_default();
            let ordinal = log.last().map(|m| m.ordinal + 1).unwrap_or(0);
            log.push(StoredMessage {
                ordinal,
                message: message.clone(),
                superseded_by: None,
            });
            Ok(())
        }

        async fn apply_session_compaction(
            &self,
            session_id: &SessionId,
            new_active: &[ChatMessage],
        ) -> StoreResult<()> {
            let mut guard = self.transcripts.lock();
            let log = guard.entry(session_id.clone()).or_default();
            let next_ordinal = log.last().map(|m| m.ordinal + 1).unwrap_or(0);
            for entry in log.iter_mut() {
                if entry.superseded_by.is_none() {
                    entry.superseded_by = Some(next_ordinal);
                }
            }
            for (offset, msg) in new_active.iter().enumerate() {
                log.push(StoredMessage {
                    ordinal: next_ordinal + offset as u64,
                    message: msg.clone(),
                    superseded_by: None,
                });
            }
            Ok(())
        }

        async fn load_active_session_messages(
            &self,
            session_id: &SessionId,
        ) -> StoreResult<Vec<ChatMessage>> {
            Ok(self
                .transcripts
                .lock()
                .get(session_id)
                .map(|log| {
                    let mut active: Vec<&StoredMessage> =
                        log.iter().filter(|m| m.superseded_by.is_none()).collect();
                    active.sort_by_key(|m| m.ordinal);
                    active.into_iter().map(|m| m.message.clone()).collect()
                })
                .unwrap_or_default())
        }
    }

    fn test_user() -> User {
        User {
            id: "user-1".to_string(),
            name: Some("Alice".to_string()),
            channel: ChannelType::tui(),
        }
    }

    #[tokio::test]
    async fn create_session_returns_valid_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        assert!(!session.id.as_str().is_empty());
        assert_eq!(session.user.id, "user-1");
        assert_eq!(session.channel, ChannelType::tui());
        assert_eq!(session.root_session_id, session.id);
        assert!(session.lineage.is_none());
    }

    #[tokio::test]
    async fn get_or_create_returns_existing_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        let session_id = session.id.clone();

        let retrieved = mgr
            .get_or_create(&session_id, test_user(), ChannelType::tui())
            .await
            .unwrap();
        assert_eq!(retrieved.id, session_id);
    }

    #[tokio::test]
    async fn get_or_create_creates_new_when_missing() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let id = SessionId::from("cli-abc");
        let session = mgr
            .get_or_create(&id, test_user(), ChannelType::tui())
            .await
            .unwrap();

        assert_eq!(session.id, id);
        let reloaded = mgr.get(&id).await.unwrap();
        assert!(reloaded.is_some());
    }

    #[tokio::test]
    async fn touch_updates_last_active() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        let original_active = session.last_active;

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        mgr.touch(&session.id).await.unwrap();

        let updated = mgr
            .get_or_create(&session.id, test_user(), ChannelType::tui())
            .await
            .unwrap();
        assert!(updated.last_active >= original_active);
    }

    #[tokio::test]
    async fn touch_nonexistent_returns_not_found() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let err = mgr
            .touch(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn cleanup_expired_removes_old_sessions() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::seconds(1));

        mgr.create_session(test_user(), ChannelType::tui())
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
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let first = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = mgr
            .create_session(test_user(), ChannelType::telegram())
            .await
            .unwrap();

        let listed = mgr.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second.id);
        assert_eq!(listed[1].id, first.id);
    }

    #[tokio::test]
    async fn history_returns_messages_for_existing_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        let messages = mgr.history(&session.id).await.unwrap();
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn history_errors_for_missing_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let err = mgr
            .history(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_removes_existing_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        mgr.delete(&session.id).await.unwrap();
        assert!(mgr.get(&session.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_errors_for_missing_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let err = mgr
            .delete(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn get_or_create_replaces_expired_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::seconds(1));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        let old_id = session.id.clone();
        let old_created = session.created_at;

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let new_session = mgr
            .get_or_create(&old_id, test_user(), ChannelType::tui())
            .await
            .unwrap();

        assert_eq!(new_session.id, old_id);
        assert!(new_session.created_at > old_created);
    }

    /// Regression for the `/compact` summary loss observed when an
    /// actor was respawned mid-session. Each message round-trips
    /// through the per-message log, and a compaction supersedes the
    /// prior rows so the active set seen on restore matches what
    /// `ContextManager` had in memory after the apply.
    #[tokio::test]
    async fn append_then_compact_round_trip() {
        use aura_model::{ChatMessage, ContentBlock, Role};

        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        let user = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("hi".into())],
        };
        let assistant = ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text("hello".into())],
        };
        mgr.append_session_message(&session.id, &user)
            .await
            .unwrap();
        mgr.append_session_message(&session.id, &assistant)
            .await
            .unwrap();

        let loaded = mgr.load_active_session_messages(&session.id).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].role, Role::User);
        assert_eq!(loaded[1].role, Role::Assistant);

        // Compaction supersedes the two prior rows and installs a
        // single summary as the new active set.
        let summary = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("<summary>S</summary>".into())],
        };
        mgr.apply_session_compaction(&session.id, std::slice::from_ref(&summary))
            .await
            .unwrap();

        let post = mgr.load_active_session_messages(&session.id).await.unwrap();
        assert_eq!(post.len(), 1);
        if let ContentBlock::Text(t) = &post[0].content[0] {
            assert!(t.contains("<summary>"));
        } else {
            panic!("expected text");
        }

        // `history()` exposes the same active slice via the public API.
        let history = mgr.history(&session.id).await.unwrap();
        assert_eq!(history.len(), 1);
    }

    #[tokio::test]
    async fn load_active_messages_empty_for_session_without_turns() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Duration::minutes(30));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        let loaded = mgr.load_active_session_messages(&session.id).await.unwrap();
        assert!(loaded.is_empty());
    }
}
