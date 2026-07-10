use async_trait::async_trait;
use baybo_model::{
    AgentFramework, AgentProfileId, ChannelType, ChatMessage, ControlEvent, ControlEventKind,
    FolderId, LineageKind, LlmEntryName, Session, SessionId,
};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// One row of `session_messages`, paired with its supersede marker.
/// Yielded by [`SessionStore::load_session_messages_with_supersede`]
/// so the trace API can replay "active as of ordinal X" filters
/// without leaking the column shape into call sites.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub ordinal: i64,
    /// `Some(n)` when a later compaction at ordinal `n` replaced this
    /// row in the active set; `None` while still active.
    pub superseded_by: Option<i64>,
    /// Wall-clock time the row was written. Used by trace hydration to
    /// detect ordinal collisions across session lifetimes: if a row's
    /// `created_at` is later than the consuming span's `started_at`,
    /// the row belongs to a different epoch and must not be returned
    /// as that span's input.
    pub created_at: DateTime<Utc>,
    pub message: ChatMessage,
}

/// Persistence interface for sessions.
///
/// `SessionId` is the caller-supplied opaque string (see
/// `baybo_model::SessionId`). Lineage relationships are stored inline on
/// the session row (`root_session_id`, `lineage_*` columns).
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get(&self, session_id: &SessionId) -> Result<Option<Session>>;
    async fn save(&self, session: &Session) -> Result<()>;
    /// Flip the `hidden` flag on the session row. Used by the
    /// chat-list "delete" UI — the row is **not** removed, just
    /// hidden from the list. Returns `Ok(true)` when the row existed
    /// and was updated, `Ok(false)` if no row matched.
    ///
    /// Implementations write only the flat `hidden` column and leave
    /// the JSON `data` blob's `hidden` field untouched: a concurrent
    /// `save` (which rewrites the full blob from a stale in-memory
    /// `Session`) would otherwise clobber the update. `get` patches
    /// `Session.hidden` from the column at read time, so observers
    /// always see the authoritative value regardless of blob
    /// staleness.
    async fn set_hidden(&self, session_id: &SessionId, hidden: bool) -> Result<bool>;

    /// Set (or clear, with `None`) the session's per-session LLM pin —
    /// the chat model switch (`PUT /v1/chat/sessions/:id/model`). Returns
    /// `Ok(true)` when the row existed and was updated, `Ok(false)` if no
    /// row matched.
    ///
    /// Same flat-column discipline as [`Self::set_hidden`]: implementations
    /// write only the `last_llm` column and leave the JSON `data` blob
    /// alone, so a concurrent `save`/`touch` (full-blob rewrite from a
    /// stale in-memory `Session`) can't clobber the pin. `get` patches
    /// `Session.state.last_llm` from the column at read time.
    async fn set_last_llm(
        &self,
        session_id: &SessionId,
        llm: Option<&LlmEntryName>,
    ) -> Result<bool>;

    /// Bind this session to an agent profile. Write-once: the SQL guard
    /// (`WHERE agent_id IS NULL`) makes a re-bind affect zero rows, so the
    /// binding is structurally immutable. `Ok(false)` = no row matched
    /// (missing id, or already bound).
    async fn set_agent_binding(
        &self,
        session_id: &SessionId,
        agent_id: &AgentProfileId,
        framework: AgentFramework,
    ) -> Result<bool>;

    /// Set (or clear, with `false`) the session's chat-list pin flag —
    /// the sidebar "pin to top" affordance (`PUT /v1/chat/sessions/:id/pin`).
    /// Returns `Ok(true)` when the row existed and was updated, `Ok(false)`
    /// if no row matched.
    ///
    /// Same flat-column discipline as [`Self::set_hidden`]: implementations
    /// write only the `pinned` column and leave the JSON `data` blob alone,
    /// so a concurrent `save`/`touch` (full-blob rewrite from a stale
    /// in-memory `Session`) can't clobber the flag. `get` patches
    /// `Session.pinned` from the column at read time.
    async fn set_pinned(&self, session_id: &SessionId, pinned: bool) -> Result<bool>;

    /// Set (or clear, with `None`) the session's chat-list folder
    /// assignment — the sidebar "move to folder" affordance
    /// (`PUT /v1/chat/sessions/:id/folder`). `None` clears it back to
    /// uncategorized. Returns `Ok(true)` when the row existed and was
    /// updated, `Ok(false)` if no row matched.
    ///
    /// Same flat-column discipline as [`Self::set_hidden`]: implementations
    /// write only the `folder_id` column and leave the JSON `data` blob
    /// alone, so a concurrent `save`/`touch` (full-blob rewrite from a stale
    /// in-memory `Session`) can't clobber the assignment. `get` patches
    /// `Session.folder_id` from the column at read time.
    async fn set_folder(
        &self,
        session_id: &SessionId,
        folder_id: Option<&FolderId>,
    ) -> Result<bool>;

    /// Advance (max-wins) the session's chat-list read cursor to `ordinal` —
    /// the highest `session_messages.ordinal` a viewer has read
    /// (`PUT /v1/chat/sessions/:id/read`). Never regresses: a lower `ordinal`
    /// is a no-op. Returns `Ok(true)` when the row existed, `Ok(false)` if no
    /// row matched. Same flat-column discipline as [`Self::set_hidden`] — the
    /// targeted UPDATE leaves the JSON `data` blob alone.
    async fn set_read_cursor(&self, session_id: &SessionId, ordinal: i64) -> Result<bool>;

    /// The session's chat-list read cursor, or `None` when nothing has been
    /// read yet (or the row is missing). Backs the list endpoint's
    /// `unread_count` derivation.
    async fn read_cursor(&self, session_id: &SessionId) -> Result<Option<i64>>;

    /// Set or clear the session's auto-generated title. Implementations
    /// must update only the flat `title` column so stale `save` calls cannot
    /// clobber it.
    async fn set_title(&self, session_id: &SessionId, title: Option<&str>) -> Result<bool>;

    /// Hard-delete the session.
    ///
    /// Returns `Ok(true)` if the row existed and was removed, `Ok(false)`
    /// if it did not exist (idempotent).
    ///
    /// Does **not** drain in-flight subagents — that is the
    /// `SessionManager`'s responsibility (cancel propagation through
    /// the actor token tree happens before this call).
    async fn delete(&self, session_id: &SessionId) -> Result<bool>;
    /// Return session ids whose `last_active` is older than `before`.
    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<SessionId>>;
    /// Return every live session, ordered by `last_active` descending.
    /// Operator-facing: drives `baybo session list`.
    async fn list_all(&self) -> Result<Vec<Session>>;

    /// Return live sessions whose `channel` equals `channel`,
    /// newest-active first. Used by the chat REST surface
    /// (`GET /v1/chat/sessions`) so a long-running gateway with
    /// thousands of telegram / weixin sessions doesn't ship every
    /// row over the wire only to discard the non-http ones in
    /// userland.
    ///
    /// Default impl is the naive `list_all() → filter` fallback so
    /// mock / in-memory stores work without overriding; the libsql
    /// impl pushes the predicate into SQL.
    async fn list_by_channel(&self, channel: &ChannelType) -> Result<Vec<Session>> {
        let all = self.list_all().await?;
        Ok(all.into_iter().filter(|s| &s.channel == channel).collect())
    }

    /// Return every immediate live descendant (by `Lineage`) of the
    /// given parent. Powers `lineage_tree` and `list_active_subagents`.
    /// Order is unspecified.
    async fn list_lineage_children(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<Vec<(SessionId, LineageKind)>>;

    /// Append one message to the session's transcript log. The store
    /// assigns the next ordinal and returns it so callers can stamp
    /// it onto live channel frames (see `Frame::Message.ordinal`),
    /// which is how clients advance their reconnect cursor.
    /// Concurrent callers on the same session must be serialized by
    /// the caller (the actor model already does this — one actor per
    /// session).
    async fn append_session_message(
        &self,
        session_id: &SessionId,
        message: &ChatMessage,
    ) -> Result<i64>;

    /// Append an out-of-band control/display event ([`ControlEvent`]) to the
    /// session — a slash-command echo or a notice. Stored separately from the
    /// transcript log (`session_control_events`); assigns the next per-session
    /// `seq` and returns it. `after_ordinal` is the `session_messages.ordinal`
    /// the event follows (`-1` if none yet), used to interleave it into the chat
    /// view; `created_at` is the event's own time (e.g. when the user hit
    /// `/stop`), shown in the UI.
    async fn append_control_event(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        kind: ControlEventKind,
        text: &str,
        created_at: DateTime<Utc>,
    ) -> Result<i64>;

    /// All control events for a session, oldest-first by `seq`.
    async fn list_control_events(&self, session_id: &SessionId) -> Result<Vec<ControlEvent>>;

    /// Apply a `/compact`-style compression: mark every currently-
    /// active row as superseded by the first newly-inserted row, then
    /// append `new_active` at the next contiguous ordinals. Atomic
    /// transaction so a partial application can never leave both the
    /// pre- and post-compaction slices marked active.
    ///
    /// `new_active` is what `ContextManager::messages()` returns
    /// after the strategy applies — i.e. the post-compression active
    /// transcript, system message included. The caller passes it
    /// through unfiltered; rows are typed by `role` so the leading
    /// system row resurfaces on the next `load_active_session_messages`.
    async fn apply_session_compaction(
        &self,
        session_id: &SessionId,
        new_active: &[ChatMessage],
    ) -> Result<()>;

    /// Load the active transcript (rows where `superseded_by IS NULL`)
    /// in ordinal order. Used by the router on actor cold start to
    /// seed `ContextManager`. Returns empty when the session has no
    /// turns yet.
    async fn load_active_session_messages(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ChatMessage>>;

    /// Highest `session_messages.ordinal` ever assigned for this
    /// session — i.e. the last row inserted, regardless of whether
    /// it has since been superseded. Used by `baybo-trace` to anchor
    /// `LlmCallInputs::Persisted` so a trace span can recover the
    /// active set the LLM saw at call time without snapshotting the
    /// messages inline. Returns `None` for a session with no rows
    /// yet.
    async fn latest_session_ordinal(&self, session_id: &SessionId) -> Result<Option<i64>>;

    /// Load every message ever appended to the session in ordinal
    /// order, paired with each row's `superseded_by` marker. Used by
    /// the trace API to hydrate `LlmCallInputs::Persisted` into the
    /// flat `Vec<ChatMessage>` shape clients still expect, applying
    /// the standard "active as of `ordinal == X`" filter on the
    /// caller's side.
    async fn load_session_messages_with_supersede(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredMessage>>;

    /// 0-indexed position of the row with `ordinal == ordinal` within
    /// the session's active sequence (rows where `superseded_by IS
    /// NULL`, in ordinal order). Returns `None` when no active row has
    /// that ordinal — either compression has rewritten it away or it
    /// never existed.
    ///
    /// Cheaper than [`Self::load_session_messages_with_supersede`] +
    /// walking: backed by a `COUNT(*)` + `EXISTS` against the partial
    /// `idx_session_messages_active` index, so message contents never
    /// cross the wire. Used by anchor-lookup paths that only need to
    /// translate a `session_summaries.cursor` into an in-memory index.
    async fn active_index_of_ordinal(
        &self,
        session_id: &SessionId,
        ordinal: i64,
    ) -> Result<Option<usize>>;

    /// Total number of active rows (`superseded_by IS NULL`) for the
    /// session. Used by drift-detection paths that compare the
    /// persisted active count to an in-memory transcript length
    /// without reading message contents.
    async fn count_active_messages(&self, session_id: &SessionId) -> Result<usize>;

    /// Active transcript with `ordinal <= up_to_ordinal`, in ordinal
    /// order. Equivalent to filtering [`Self::load_active_session_messages`]
    /// by ordinal but pushes the predicate into SQL so the row content
    /// for newer ordinals never crosses the wire. Used by background
    /// compression to load the snapshot pinned at trigger time.
    async fn load_active_session_messages_up_to(
        &self,
        session_id: &SessionId,
        up_to_ordinal: i64,
    ) -> Result<Vec<ChatMessage>>;

    /// Reverse-paginated slice of the active transcript: at most
    /// `limit` rows whose `ordinal` is strictly below `before_ordinal`
    /// (or the tail of the transcript when `before_ordinal` is `None`),
    /// returned in **ascending** ordinal order. Each row is paired with
    /// its absolute ordinal and persisted `created_at` so the caller
    /// can both request the next-older page and render a per-message
    /// timestamp without a second lookup.
    ///
    /// Used by the chat REST surface so a long-running session doesn't
    /// pay an O(transcript-length) round-trip on every initial load;
    /// the web client streams older slices in on scroll-up.
    async fn load_active_session_messages_tail(
        &self,
        session_id: &SessionId,
        before_ordinal: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, DateTime<Utc>, ChatMessage)>>;

    /// Forward difference slice: the next at most `limit` active rows
    /// whose `ordinal` is strictly **greater than** `after_ordinal`,
    /// returned in ascending order alongside each row's absolute
    /// ordinal and persisted `created_at`. Powers the REST sync
    /// endpoint's cursor scan — the one forward-recovery pull a chat
    /// client runs after any gap.
    async fn load_active_session_messages_since(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        limit: usize,
    ) -> Result<Vec<(i64, DateTime<Utc>, ChatMessage)>>;

    /// Ordinal of the newest persisted row carrying this
    /// `platform_msg_id` (the client-generated send idempotency key),
    /// superseded rows included — a compacted-away row still proves the
    /// send was durably persisted. `None` when no row matches. Powers
    /// the outbox point lookup that resolves a rebase-floor entry
    /// without consuming a retry transmission.
    async fn find_message_ordinal_by_platform_msg_id(
        &self,
        session_id: &SessionId,
        platform_msg_id: &str,
    ) -> Result<Option<i64>>;

    /// The freshest **human-authored** active message — source `user` or
    /// `user_interjection` (i.e. [`ChatMessage::from_user`]) — paired with
    /// its persisted `created_at`, or `None` when the session has no such
    /// turn. A single indexed `ORDER BY ordinal DESC LIMIT 1` lookup;
    /// powers the chat sidebar preview so a prompt buried under a long
    /// tool loop is found without walking the tail.
    async fn load_last_user_message(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(DateTime<Utc>, ChatMessage)>>;
}
