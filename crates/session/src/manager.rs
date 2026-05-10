use std::sync::Arc;

use aura_model::{ChannelType, ChatMessage, Session, SessionId, SessionState, TriggerSource, User};
use aura_storage::{SessionStore, SessionSummaryRow, SessionSummaryStore, StorageError};
use chrono::{DateTime, Duration, Utc};
use tracing::{debug, warn};

use crate::SessionError;

type Result<T> = std::result::Result<T, SessionError>;

fn wrap(e: StorageError) -> SessionError {
    SessionError::Storage(e.to_string())
}

/// Higher-level session management logic wrapping a `SessionStore`.
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
    /// Per-session summary-metadata store. Required at construction —
    /// production wires the libsql backend; tests pass
    /// `aura_storage::test_support::MemorySessionSummaryStore`.
    summary_store: Arc<dyn SessionSummaryStore>,
    session_timeout: Duration,
    /// Default soul version stamped on new sessions when the caller
    /// does not supply one. The agent layer overrides this via
    /// `create_session_with_options` once it loads the live config.
    default_soul_version: String,
}

impl SessionManager {
    pub fn new(
        store: Arc<dyn SessionStore>,
        summary_store: Arc<dyn SessionSummaryStore>,
        session_timeout: Duration,
    ) -> Self {
        Self {
            store,
            summary_store,
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

    /// Read-only handle to the underlying `SessionSummaryStore`. Used
    /// by callers that need direct access (orphan reaper's FS sweep
    /// over `list_session_ids`, query-layer joins).
    pub fn summary_store(&self) -> Arc<dyn SessionSummaryStore> {
        Arc::clone(&self.summary_store)
    }

    /// Read this session's summary-metadata row. `Ok(None)` when the
    /// session has never had a summary pass written.
    pub async fn summary_metadata(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummaryRow>> {
        self.summary_store.get(session_id).await.map_err(wrap)
    }

    /// Record a successful summary pass: bumps `pass_count`, advances
    /// `cursor`, accumulates cost, resets `error_count` to 0.
    pub async fn record_summary_success(
        &self,
        session_id: &SessionId,
        cursor: i64,
        cost_micros_delta: i64,
        model_id: &str,
        span_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        self.summary_store
            .upsert_success(
                session_id,
                cursor,
                cost_micros_delta,
                model_id,
                span_id,
                updated_at,
            )
            .await
            .map_err(wrap)
    }

    /// Record a failed summary attempt: bumps `error_count`. Inserts
    /// a row with cursor=0 / pass_count=0 if absent so failure
    /// telemetry surfaces even on sessions that have never had a
    /// successful pass.
    pub async fn record_summary_failure(
        &self,
        session_id: &SessionId,
        model_id: &str,
        span_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        self.summary_store
            .bump_error_count(session_id, model_id, span_id, updated_at)
            .await
            .map_err(wrap)
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

    /// Create a `LineageKind::SystemMaintenance` session for
    /// internal maintenance work on behalf of `parent`. Unlike
    /// [`Self::create_spawned_session`], the trigger is **not**
    /// inherited — maintenance work always carries
    /// `TriggerSource::System { reason }` so cost / trace
    /// attribution shows it as system-driven, not user-driven.
    /// `root_session_id` *is* inherited from the parent so
    /// "all work descended from session X" queries surface
    /// maintenance work too.
    ///
    /// The `is_normal_session = 0` flag is set automatically by the
    /// `LineageKind::SystemMaintenance` branch in
    /// `LibsqlSessionStore::save`, so default queries (CLI session
    /// picker, web UI) skip these rows. `parent_job_id` pins the
    /// exact moment in the parent's lifeline that the spawn
    /// happened — same role it plays for subagents.
    ///
    /// Used today by the async summary-refresh path
    /// (`SystemReason::BackgroundCompression`); future maintenance reasons
    /// can reuse the same constructor without a code change.
    pub async fn create_maintenance_session(
        &self,
        parent: &Session,
        parent_job_id: aura_model::JobId,
        reason: aura_model::SystemReason,
    ) -> Result<Session> {
        let id = SessionId::from(format!("maint-{}", uuid::Uuid::new_v4()));
        let now = Utc::now();
        let session = Session {
            id: id.clone(),
            user: parent.user.clone(),
            channel: parent.channel.clone(),
            created_at: now,
            last_active: now,
            state: aura_model::SessionState::default(),
            root_session_id: parent.root_session_id.clone(),
            trigger: TriggerSource::System { reason },
            lineage: Some(aura_model::Lineage {
                parent_session_id: parent.id.clone(),
                parent_job_id,
                kind: aura_model::LineageKind::SystemMaintenance,
            }),
            bound_soul_version: parent.bound_soul_version.clone(),
        };
        self.store.save(&session).await.map_err(wrap)?;
        debug!(
            session_id = %session.id,
            parent_session_id = %parent.id,
            "spawned system maintenance session"
        );
        Ok(session)
    }

    /// Return ids of in-flight maintenance sessions whose lineage
    /// points at `parent_session_id`. Used by the parent's agent
    /// loop to enforce the at-most-one-in-flight `BackgroundCompressionRunner`
    /// invariant before spawning a new pass — if any row is
    /// returned, skip the trigger (the in-flight pass will eventually
    /// land or be reaped on next startup).
    pub async fn active_maintenance_for_parent(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<Vec<SessionId>> {
        self.store
            .list_active_maintenance_for_parent(parent_session_id)
            .await
            .map_err(wrap)
    }

    /// Enumerate every maintenance session id (`is_normal_session
    /// = 0`). Used by the startup orphan reaper to delete sessions
    /// whose actor isn't running after a process bounce.
    pub async fn all_maintenance_sessions(&self) -> Result<Vec<SessionId>> {
        self.store
            .list_all_maintenance_sessions()
            .await
            .map_err(wrap)
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
            aura_model::LineageKind::SystemMaintenance => "maint-",
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

    /// Highest `session_messages.ordinal` ever assigned for the
    /// session, regardless of supersede status. Used by the trace
    /// span builder to anchor `LlmCallInputs::Persisted`.
    pub async fn latest_session_ordinal(&self, session_id: &SessionId) -> Result<Option<i64>> {
        self.store
            .latest_session_ordinal(session_id)
            .await
            .map_err(wrap)
    }

    /// Full message log including superseded rows, paired with each
    /// row's `superseded_by` marker. Used by the trace replay path
    /// to reconstruct active slices "as of ordinal X" for older
    /// `LlmCall` spans.
    pub async fn load_session_messages_with_supersede(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<aura_storage::StoredMessage>> {
        self.store
            .load_session_messages_with_supersede(session_id)
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
    use std::sync::Arc;

    use aura_model::{ChannelType, SessionId, User};
    use aura_storage::test_support::{MemorySessionStore, MemorySessionSummaryStore};
    use chrono::{Duration, Utc};

    use super::{SessionError, SessionManager, SessionStore};

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
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

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
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

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
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

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
        let mgr = SessionManager::new(
            store.clone(),
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

        let mut session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        // Back-date so `touch` produces a strictly newer timestamp without sleeping.
        session.last_active = Utc::now() - Duration::seconds(5);
        store.save(&session).await.unwrap();
        let original_active = session.last_active;

        mgr.touch(&session.id).await.unwrap();

        let updated = mgr
            .get_or_create(&session.id, test_user(), ChannelType::tui())
            .await
            .unwrap();
        assert!(updated.last_active > original_active);
    }

    #[tokio::test]
    async fn touch_nonexistent_returns_not_found() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

        let err = mgr
            .touch(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn cleanup_expired_removes_old_sessions() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(
            store.clone(),
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::seconds(1),
        );

        let mut session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        // Back-date past the 1s timeout instead of sleeping.
        session.last_active = Utc::now() - Duration::seconds(5);
        store.save(&session).await.unwrap();

        let count = mgr.cleanup_expired().await.unwrap();
        assert_eq!(count, 1);

        let count = mgr.cleanup_expired().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn list_returns_all_sessions_newest_first() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(
            store.clone(),
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

        let mut first = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        // Back-date `first` so the sort order is deterministic without sleeping.
        first.last_active = Utc::now() - Duration::seconds(5);
        store.save(&first).await.unwrap();
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
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

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
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

        let err = mgr
            .history(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_removes_existing_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

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
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

        let err = mgr
            .delete(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn get_or_create_replaces_expired_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(
            store.clone(),
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::seconds(1),
        );

        let mut session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        let old_id = session.id.clone();
        // Back-date both timestamps so the manager treats it as expired
        // and the replacement's `created_at` is strictly newer.
        let backdate = Utc::now() - Duration::seconds(5);
        session.created_at = backdate;
        session.last_active = backdate;
        store.save(&session).await.unwrap();
        let old_created = session.created_at;

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
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

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
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        let loaded = mgr.load_active_session_messages(&session.id).await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn create_maintenance_session_carries_system_trigger_and_maintenance_lineage() {
        use aura_model::{JobId, LineageKind, SystemReason};

        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(
            store,
            Arc::new(MemorySessionSummaryStore::new()),
            Duration::minutes(30),
        );

        let parent = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        let parent_job = JobId::new();

        let maint = mgr
            .create_maintenance_session(&parent, parent_job, SystemReason::BackgroundCompression)
            .await
            .unwrap();

        // System trigger + reason — not inherited from parent.
        match &maint.trigger {
            aura_model::TriggerSource::System { reason } => {
                assert_eq!(*reason, SystemReason::BackgroundCompression);
            }
            other => panic!("expected System trigger, got {other:?}"),
        }
        // Lineage points back at the parent through SystemMaintenance.
        let lineage = maint.lineage.as_ref().expect("maintenance has lineage");
        assert_eq!(lineage.parent_session_id, parent.id);
        assert_eq!(lineage.parent_job_id, parent_job);
        assert!(matches!(lineage.kind, LineageKind::SystemMaintenance));
        // Root inherits from the parent (not self) so descendant
        // queries cover maintenance work.
        assert_eq!(maint.root_session_id, parent.root_session_id);
        // Maintenance ids carry the `maint-` prefix for log
        // recognisability.
        assert!(maint.id.as_str().starts_with("maint-"));
    }
}
