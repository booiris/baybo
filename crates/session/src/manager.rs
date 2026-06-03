use std::sync::Arc;

use aura_model::{
    ChannelType, ChatMessage, LlmEntryName, Session, SessionId, SessionState, TriggerSource, User,
};
use chrono::{DateTime, Duration, Utc};
use tracing::{debug, warn};

use crate::SessionError;
use aura_store::{SessionStore, StoredMessage};
use aura_store::{SessionSummaryRow, SessionSummaryStore};

type Result<T> = std::result::Result<T, SessionError>;

/// Higher-level session management logic wrapping a `SessionStore`.
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
    /// Per-session summary-metadata store. Required at construction —
    /// production wires the libsql backend; tests pass
    /// `crate::test_support::MemorySessionSummaryStore`.
    summary_store: Arc<dyn SessionSummaryStore>,
    /// Default soul version stamped on new sessions when the caller
    /// does not supply one. The agent layer overrides this via
    /// `create_session_with_options` once it loads the live config.
    default_soul_version: String,
}

impl SessionManager {
    pub fn new(store: Arc<dyn SessionStore>, summary_store: Arc<dyn SessionSummaryStore>) -> Self {
        Self {
            store,
            summary_store,
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
        self.summary_store
            .get(session_id)
            .await
            .map_err(SessionError::from)
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
            .map_err(SessionError::from)
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
            .map_err(SessionError::from)
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
        };
        let id = SessionId::from(format!("{prefix}{}", uuid::Uuid::new_v4()));
        self.create_session_with_id(id, user, channel, trigger)
            .await
    }

    /// Create a session that descends from `parent` via the given
    /// lineage (subagent). The child inherits its trigger from the
    /// parent's root session and gets a fresh session_id prefixed
    /// with `subagent-` as a hint.
    pub async fn create_spawned_session(
        &self,
        user: User,
        channel: ChannelType,
        parent: &Session,
        lineage: aura_model::Lineage,
    ) -> Result<Session> {
        let prefix = match lineage.kind {
            aura_model::LineageKind::Subagent => "subagent-",
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
            hidden: false,
        };
        self.store.save(&session).await?;
        debug!(
            session_id = %session.id,
            parent_session_id = %parent.id,
            "spawned subagent session"
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
            hidden: false,
        };
        self.store.save(&session).await?;
        debug!(session_id = %session.id, "created new session");
        Ok(session)
    }

    pub async fn get_or_create(
        &self,
        session_id: &SessionId,
        user: User,
        channel: ChannelType,
    ) -> Result<Session> {
        if let Some(session) = self.store.get(session_id).await? {
            // Idle sessions are NOT deleted on access. Deleting the
            // session row would cascade to `session_messages` and
            // strand the still-existing `jobs` / `steps` / `spans`
            // rows pointing at ordinals that get reused by the new
            // lifetime — trace replay then hydrates LLM spans against
            // a different epoch's transcript and shows misleading
            // content. Treat the expiry timeout as an in-memory
            // signal only (router/supervisor evict the actor); the
            // persisted history stays intact.
            debug!(session_id = %session_id, "returning existing session");
            return Ok(session);
        }
        debug!(session_id = %session_id, "session not found, creating new session");
        self.create_session_with_id(session_id.clone(), user, channel, TriggerSource::User)
            .await
    }

    pub async fn get(&self, session_id: &SessionId) -> Result<Option<Session>> {
        self.store.get(session_id).await.map_err(SessionError::from)
    }

    /// Return every session known to the underlying store, newest-active first.
    pub async fn list(&self) -> Result<Vec<Session>> {
        let mut sessions = self.store.list_all().await?;
        sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        Ok(sessions)
    }

    /// Live sessions whose `channel` matches `channel`, newest-active
    /// first. Delegates to the store's `list_by_channel`, which
    /// pushes the predicate into SQL on the libsql backend so a
    /// long-running gateway with thousands of bot sessions doesn't
    /// pay an O(all) round-trip when the caller only wants the
    /// http channel.
    pub async fn list_by_channel(&self, channel: &aura_model::ChannelType) -> Result<Vec<Session>> {
        let mut sessions = self.store.list_by_channel(channel).await?;
        sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        Ok(sessions)
    }

    /// Return the active transcript for the session — non-superseded
    /// rows of the append-only `session_messages` log, in order.
    /// Errors with `SessionError::NotFound` if the session itself
    /// does not exist; an existing session with no turns yet returns
    /// an empty vector.
    pub async fn history(&self, session_id: &SessionId) -> Result<Vec<ChatMessage>> {
        if self.store.get(session_id).await?.is_none() {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        self.store
            .load_active_session_messages(session_id)
            .await
            .map_err(SessionError::from)
    }

    /// Return EVERY message ever appended to the session — including rows
    /// later marked `superseded_by` (i.e. rewritten by compaction), in
    /// original ordinal order. Use this when a caller needs the raw
    /// user/assistant turns instead of the compressed active view that
    /// [`Self::history`] returns: today the only consumer is
    /// `Memory::on_session_end`, whose contract is the full transcript
    /// before any compression folded earlier turns into a summary. Errors
    /// with `SessionError::NotFound` if the session row is missing.
    pub async fn full_transcript(&self, session_id: &SessionId) -> Result<Vec<ChatMessage>> {
        if self.store.get(session_id).await?.is_none() {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        let rows = self
            .store
            .load_session_messages_with_supersede(session_id)
            .await
            .map_err(SessionError::from)?;
        Ok(rows.into_iter().map(|r| r.message).collect())
    }

    /// Reverse-paginated slice of the active transcript: the
    /// most-recent `limit` rows whose `ordinal` is strictly below
    /// `before_ordinal` (or the tail when `before_ordinal` is `None`),
    /// returned in ascending ordinal order paired with each row's
    /// absolute ordinal and persisted `created_at`. Errors with
    /// `SessionError::NotFound` when the session itself does not
    /// exist. Used by the chat REST surface to avoid round-tripping
    /// a long-running session's full transcript on every initial load
    /// while still letting the client render per-message timestamps.
    pub async fn history_tail(
        &self,
        session_id: &SessionId,
        before_ordinal: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, DateTime<Utc>, ChatMessage)>> {
        if self.store.get(session_id).await?.is_none() {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        self.store
            .load_active_session_messages_tail(session_id, before_ordinal, limit)
            .await
            .map_err(SessionError::from)
    }

    /// Forward catch-up slice: at most `limit` active rows with
    /// `ordinal > after_ordinal`, ascending. Powers the WS
    /// `Subscribe { since_ordinal }` cursor — the gateway streams the
    /// result back as `Frame::Message` to a reconnecting client that
    /// missed some persisted rows. `SessionError::NotFound` when the
    /// session itself does not exist; an empty slice is the legitimate
    /// "nothing missed" answer.
    pub async fn history_since(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        limit: usize,
    ) -> Result<Vec<(i64, ChatMessage)>> {
        if self.store.get(session_id).await?.is_none() {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        self.store
            .load_active_session_messages_since(session_id, after_ordinal, limit)
            .await
            .map_err(SessionError::from)
    }

    /// The freshest human-authored message (source `user` /
    /// `user_interjection`), paired with its persisted `created_at`, or
    /// `None`. A single indexed lookup — powers the chat sidebar preview
    /// without walking the tail, so a prompt buried under a long tool loop
    /// is still found. No existence pre-check: an unknown session and one
    /// with no user turn both yield `None` (both render no preview).
    pub async fn last_user_message(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(DateTime<Utc>, ChatMessage)>> {
        self.store
            .load_last_user_message(session_id)
            .await
            .map_err(SessionError::from)
    }

    /// Append a single message to the session's transcript log.
    /// Called by `AgentLoop` from the `&mut self` append paths so
    /// every turn's worth of messages reaches storage incrementally
    /// rather than via a per-turn full-blob rewrite. Returns the
    /// ordinal the store assigned to this row — callers stamp it onto
    /// the outbound `Frame::Message` so reconnecting clients can
    /// advance their cursor past live emissions.
    pub async fn append_session_message(
        &self,
        session_id: &SessionId,
        message: &ChatMessage,
    ) -> Result<i64> {
        self.store
            .append_session_message(session_id, message)
            .await
            .map_err(SessionError::from)
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
            .map_err(SessionError::from)
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
            .map_err(SessionError::from)
    }

    /// Highest `session_messages.ordinal` ever assigned for the
    /// session, regardless of supersede status. Used by the trace
    /// span builder to anchor `LlmCallInputs::Persisted`.
    pub async fn latest_session_ordinal(&self, session_id: &SessionId) -> Result<Option<i64>> {
        self.store
            .latest_session_ordinal(session_id)
            .await
            .map_err(SessionError::from)
    }

    /// Full message log including superseded rows, paired with each
    /// row's `superseded_by` marker. Used by the trace replay path
    /// to reconstruct active slices "as of ordinal X" for older
    /// `LlmCall` spans.
    pub async fn load_session_messages_with_supersede(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredMessage>> {
        self.store
            .load_session_messages_with_supersede(session_id)
            .await
            .map_err(SessionError::from)
    }

    /// 0-indexed position of `ordinal` within the active message
    /// sequence; `None` if the row is no longer active. Index-only
    /// — does not read message content.
    pub async fn active_index_of_ordinal(
        &self,
        session_id: &SessionId,
        ordinal: i64,
    ) -> Result<Option<usize>> {
        self.store
            .active_index_of_ordinal(session_id, ordinal)
            .await
            .map_err(SessionError::from)
    }

    /// Count of active rows for the session. Index-only.
    pub async fn count_active_messages(&self, session_id: &SessionId) -> Result<usize> {
        self.store
            .count_active_messages(session_id)
            .await
            .map_err(SessionError::from)
    }

    /// Active transcript clipped to `ordinal <= up_to_ordinal`. Used
    /// by background compression to load the snapshot pinned at
    /// trigger time without dragging the post-snapshot tail across
    /// the wire.
    pub async fn load_active_session_messages_up_to(
        &self,
        session_id: &SessionId,
        up_to_ordinal: i64,
    ) -> Result<Vec<ChatMessage>> {
        self.store
            .load_active_session_messages_up_to(session_id, up_to_ordinal)
            .await
            .map_err(SessionError::from)
    }

    /// Hard-delete a session by id. Errors with `SessionError::NotFound`
    /// if the session did not exist at the time of the call.
    pub async fn delete(&self, session_id: &SessionId) -> Result<()> {
        let deleted = self.store.delete(session_id).await?;
        if !deleted {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        debug!(session_id = %session_id, "deleted session");
        Ok(())
    }

    /// Flip the session's `hidden` flag. The row is **not** removed
    /// — the chat list API filters `hidden = true` out, but the
    /// session itself, its transcript, channel token, and any
    /// running actor stay live. Returns `Err(NotFound)` when the
    /// session id is unknown.
    pub async fn set_hidden(&self, session_id: &SessionId, hidden: bool) -> Result<()> {
        let updated = self.store.set_hidden(session_id, hidden).await?;
        if !updated {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        debug!(session_id = %session_id, hidden, "toggled session hidden");
        Ok(())
    }

    /// Set (or clear, with `None`) the session's per-session LLM pin —
    /// the chat model switch. Targeted flat-column write that survives a
    /// concurrent `touch`; see [`aura_store::SessionStore::set_last_llm`].
    /// Returns `Err(NotFound)` when the session id is unknown.
    pub async fn set_last_llm(
        &self,
        session_id: &SessionId,
        llm: Option<&LlmEntryName>,
    ) -> Result<()> {
        let updated = self.store.set_last_llm(session_id, llm).await?;
        if !updated {
            return Err(SessionError::NotFound(format!("session {session_id}")));
        }
        debug!(session_id = %session_id, llm = ?llm, "set session llm pin");
        Ok(())
    }

    pub async fn touch(&self, session_id: &SessionId) -> Result<()> {
        let session = self.store.get(session_id).await?;
        match session {
            Some(mut session) => {
                session.last_active = Utc::now();
                self.store.save(&session).await?;
                debug!(session_id = %session_id, "touched session");
                Ok(())
            }
            None => {
                warn!(session_id = %session_id, "attempted to touch non-existent session");
                Err(SessionError::NotFound(format!("session {session_id}")))
            }
        }
    }

    /// List candidate idle sessions: every "normal" session whose
    /// `last_active` is older than the supplied threshold. Surfaces
    /// session IDs the actor reaper should consider for eviction.
    /// Hydration on the next user message restores the transcript from
    /// store, so dropping the in-memory actor is lossless as long as
    /// no turn is in flight (the invariant the caller is expected to
    /// uphold).
    ///
    /// This method must **never** become a delete. Session conversations
    /// are core user data; see CLAUDE.md ("Session data is core data").
    pub async fn idle_sessions(&self, threshold: Duration) -> Result<Vec<SessionId>> {
        let cutoff = Utc::now() - threshold;
        self.store
            .list_expired(cutoff)
            .await
            .map_err(SessionError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aura_model::{ChannelType, SessionId, User};
    use chrono::{Duration, Utc};

    use super::{SessionError, SessionManager, SessionStore};
    use crate::test_support::{MemorySessionStore, MemorySessionSummaryStore};

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
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

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
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

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
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

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
        let mgr = SessionManager::new(store.clone(), Arc::new(MemorySessionSummaryStore::new()));

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
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

        let err = mgr
            .touch(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn idle_sessions_lists_without_deleting() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store.clone(), Arc::new(MemorySessionSummaryStore::new()));

        let mut idle = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        idle.last_active = Utc::now() - Duration::seconds(120);
        store.save(&idle).await.unwrap();

        let fresh = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        let idle_ids = mgr.idle_sessions(Duration::seconds(60)).await.unwrap();
        assert_eq!(idle_ids, vec![idle.id.clone()]);

        // List-only — neither session is deleted.
        assert!(store.get(&idle.id).await.unwrap().is_some());
        assert!(store.get(&fresh.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn list_returns_all_sessions_newest_first() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store.clone(), Arc::new(MemorySessionSummaryStore::new()));

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
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

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
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

        let err = mgr
            .history(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn full_transcript_surfaces_superseded_rows_that_history_hides() {
        // The whole reason `full_transcript` exists separately from
        // `history`: `Memory::on_session_end` needs the raw user/assistant
        // turns even after compaction has folded them into a summary.
        // `history` filters by `superseded_by IS NULL` and would hide
        // the original three turns; `full_transcript` returns every row
        // ever appended.
        use aura_model::{ChatMessage, ContentBlock};

        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store.clone(), Arc::new(MemorySessionSummaryStore::new()));
        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        // Three original turns get appended at ordinals 0/1/2.
        for text in ["one", "two", "three"] {
            let msg = ChatMessage::user(vec![ContentBlock::Text(text.into())]);
            store
                .append_session_message(&session.id, &msg)
                .await
                .unwrap();
        }
        // Compact: marks the three originals as superseded and appends a
        // single summary row at ordinal 3.
        let summary = ChatMessage::system(vec![ContentBlock::Text("summary".into())]);
        store
            .apply_session_compaction(&session.id, &[summary])
            .await
            .unwrap();

        let active = mgr.history(&session.id).await.unwrap();
        assert_eq!(
            active.len(),
            1,
            "history filters to the active (post-compaction) row",
        );

        let full = mgr.full_transcript(&session.id).await.unwrap();
        assert_eq!(
            full.len(),
            4,
            "full_transcript returns 3 superseded originals + 1 active summary",
        );
        assert_eq!(
            full.iter()
                .filter_map(|m| match m.content.first() {
                    Some(ContentBlock::Text(t)) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["one", "two", "three", "summary"],
        );
    }

    #[tokio::test]
    async fn full_transcript_errors_for_missing_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

        let err = mgr
            .full_transcript(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_removes_existing_session() {
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

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
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

        let err = mgr
            .delete(&SessionId::from("nonexistent"))
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    #[tokio::test]
    async fn get_or_create_keeps_idle_session_intact() {
        // Idle past the timeout must NOT delete the persisted session
        // row — cascading the cleanup would also wipe `session_messages`
        // and strand existing `jobs` / `steps` / `spans` rows that
        // reference ordinals from this lifetime. The expiry signal is
        // in-memory only (router/supervisor drop the actor); history
        // stays put.
        let store = Arc::new(MemorySessionStore::new());
        let mgr = SessionManager::new(store.clone(), Arc::new(MemorySessionSummaryStore::new()));

        let mut session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();
        let old_id = session.id.clone();
        let backdate = Utc::now() - Duration::seconds(5);
        session.created_at = backdate;
        session.last_active = backdate;
        store.save(&session).await.unwrap();
        let old_created = session.created_at;

        let returned = mgr
            .get_or_create(&old_id, test_user(), ChannelType::tui())
            .await
            .unwrap();

        assert_eq!(returned.id, old_id);
        assert_eq!(returned.created_at, old_created);
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
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        let user = ChatMessage::agent_context(vec![ContentBlock::Text("hi".into())]);
        let assistant = ChatMessage::assistant(vec![ContentBlock::Text("hello".into())]);
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
        let summary =
            ChatMessage::agent_context(vec![ContentBlock::Text("<summary>S</summary>".into())]);
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
        let mgr = SessionManager::new(store, Arc::new(MemorySessionSummaryStore::new()));

        let session = mgr
            .create_session(test_user(), ChannelType::tui())
            .await
            .unwrap();

        let loaded = mgr.load_active_session_messages(&session.id).await.unwrap();
        assert!(loaded.is_empty());
    }
}
