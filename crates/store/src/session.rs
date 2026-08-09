use async_trait::async_trait;
use baybo_model::{
    AgentProfileId, ChannelType, ChatMessage, ControlEvent, ControlEventKind, FolderId,
    LineageKind, LlmEntryName, Session, SessionId,
};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// A conversation the dream pass has not consolidated yet, as
/// [`SessionStore::dream_candidates`] reports it.
///
/// Flat projection: no session blob is decoded, because the caller only
/// needs enough to decide whether the transcript is worth reading and which
/// agent's memory it belongs to.
#[derive(Debug, Clone)]
pub struct DreamCandidate {
    pub session_id: SessionId,
    /// The agent this session was bound to. `None` is the built-in agent —
    /// the same "NULL means built-in" rule the column carries everywhere.
    pub agent_id: Option<AgentProfileId>,
    pub title: Option<String>,
    /// Newest unconsolidated row, whoever wrote it.
    pub last_activity_at: DateTime<Utc>,
    /// How many of the unconsolidated rows a human wrote — a cheap proxy
    /// for "how much happened here", and zero for a session whose only new
    /// rows are the agent's (a background delivery landing long after the
    /// conversation went quiet).
    pub human_message_count: i64,
    /// Ordinal to start reading at: the oldest unconsolidated row.
    ///
    /// Without it a long-lived session is re-read from row 0 every pass —
    /// selection says *which* conversations to read, not how much of each,
    /// and a default-paginated read of a months-old transcript returns its
    /// opening pages, the part already consolidated, while the new activity
    /// sits past the end of the page.
    pub read_from_ordinal: i64,
    /// Highest ordinal in this session as of the query — what the pass's
    /// cursor advances to once the conversation has been offered.
    pub latest_ordinal: i64,
}

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
    /// `true` for a row `apply_session_compaction` wrote (summary head +
    /// re-injected recent turns). Trace hydration ignores it — the
    /// machinery IS the model's context — but the chat display reads use
    /// it to render the real conversation once (see
    /// [`SessionStore::load_active_session_messages_tail`]).
    pub compaction_inserted: bool,
    pub message: ChatMessage,
}

/// Result of appending a transcript row under a durable source-event key.
/// `Existing` means the same source event already owns a row in this session,
/// so callers must not mirror another copy into their in-memory transcript.
/// The caller still decides whether the surrounding workflow is complete or
/// must resume from that durable row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMessageAppendOutcome {
    Inserted { ordinal: i64 },
    Existing { ordinal: i64 },
}

impl SessionMessageAppendOutcome {
    pub fn ordinal(self) -> i64 {
        match self {
            Self::Inserted { ordinal } | Self::Existing { ordinal } => ordinal,
        }
    }

    pub fn was_inserted(self) -> bool {
        matches!(self, Self::Inserted { .. })
    }
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

    /// Set (or clear, with `None`) the session's per-session MODEL pick
    /// within [`Self::set_last_llm`]'s entry — a `model_list` id.
    /// Same flat-column discipline as [`Self::set_last_llm`]: writes only
    /// the `last_model` column, leaving the JSON `data` blob alone. `get`
    /// patches `Session.state.last_model` from the column at read time.
    /// `None` clears the pick back to the entry's default model.
    async fn set_last_model(&self, session_id: &SessionId, model: Option<&str>) -> Result<bool>;

    /// Set (or clear, with `None`) the session's per-session reasoning-effort
    /// pin. Same flat-column discipline as [`Self::set_last_model`]: writes
    /// only the `last_effort` column. `get` patches
    /// `Session.state.last_effort` at read time; `None` clears it to the
    /// entry's default effort.
    async fn set_last_effort(&self, session_id: &SessionId, effort: Option<&str>) -> Result<bool>;

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

    /// Set (or clear, with `false`) the session's chat-list archive flag
    /// (`PUT /v1/chat/sessions/:id/archive`). Presentation only — the
    /// list surfaces keep returning archived rows and clients group them;
    /// new activity never clears the flag. Returns `Ok(true)` when the
    /// row existed and was updated, `Ok(false)` if no row matched.
    ///
    /// Same flat-column discipline as [`Self::set_hidden`]: implementations
    /// write only the `archived` column and leave the JSON `data` blob
    /// alone, so a concurrent `save`/`touch` (full-blob rewrite from a
    /// stale in-memory `Session`) can't clobber the flag. `get` patches
    /// `Session.archived` from the column at read time.
    async fn set_archived(&self, session_id: &SessionId, archived: bool) -> Result<bool>;

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

    async fn list_project_conversations(&self, project_id: &str) -> Result<Vec<Session>>;

    /// Return live sessions whose `channel` equals `channel`,
    /// newest-active first. Used by the chat REST surface
    /// (`GET /v1/chat/sessions`) so a long-running gateway with
    /// thousands of telegram / weixin sessions doesn't ship every
    /// row over the wire only to discard the non-http ones in
    /// userland.
    ///
    /// Default impl is the naive `list_all() → filter` fallback so
    /// mock / in-memory stores work without overriding; the sqlite
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

    /// Append one message under a non-empty source-event id. The
    /// `(session_id, source_event_id)` pair is unique across the complete
    /// transcript, including superseded rows. Implementations atomically
    /// return the existing ordinal instead of inserting a duplicate.
    async fn append_session_message_idempotent(
        &self,
        session_id: &SessionId,
        source_event_id: &str,
        message: &ChatMessage,
    ) -> Result<SessionMessageAppendOutcome>;

    /// Look up the transcript row already claimed by a source-event id.
    async fn find_message_ordinal_by_source_event_id(
        &self,
        session_id: &SessionId,
        source_event_id: &str,
    ) -> Result<Option<i64>>;

    /// Append an out-of-band control/display event ([`ControlEvent`]) to the
    /// session — a slash-command echo or a notice. Stored separately from the
    /// transcript log (`session_control_events`); assigns the next per-session
    /// `seq` and returns it. `after_ordinal` is the `session_messages.ordinal`
    /// the event follows (`-1` if none yet), used to interleave it into the chat
    /// view; `created_at` is the event's own time (e.g. when the user hit
    /// `/stop`), shown in the UI. `platform_msg_id` is the originating send's id
    /// for a user-authored `Command` echo (empty for notices) so a client's
    /// optimistic command bubble reconciles with the durable echo.
    async fn append_control_event(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        kind: ControlEventKind,
        text: &str,
        created_at: DateTime<Utc>,
        platform_msg_id: &str,
    ) -> Result<i64>;

    /// All control events for a session, oldest-first by `seq`.
    async fn list_control_events(&self, session_id: &SessionId) -> Result<Vec<ControlEvent>>;
    /// Control events whose `after_ordinal` anchor falls in
    /// `[lower, upper]`, seq-ordered — the page-scoped variant of
    /// [`Self::list_control_events`] so a history page never loads a
    /// session's whole event log to keep a slice.
    async fn list_control_events_in_range(
        &self,
        session_id: &SessionId,
        lower: i64,
        upper: i64,
    ) -> Result<Vec<ControlEvent>>;

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
    ///
    /// Returns the ordinal of the FIRST newly-inserted row; `new_active`
    /// lands contiguously, so `base + i` addresses row `i`.
    async fn apply_session_compaction(
        &self,
        session_id: &SessionId,
        new_active: &[ChatMessage],
    ) -> Result<i64>;

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

    /// Incremental variant of
    /// [`Self::load_session_messages_with_supersede`]: only rows with
    /// `ordinal > after_ordinal`, superseded markers included. Pollers
    /// that already hold a prefix use this instead of re-reading the
    /// whole transcript; they must also watch
    /// [`Self::supersede_watermark`] to learn when the prefix's own
    /// markers went stale.
    async fn load_session_messages_with_supersede_since(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
    ) -> Result<Vec<StoredMessage>>;

    /// Highest `superseded_by` marker across the session (`None` while
    /// nothing is superseded). It only ever advances, and only when a
    /// compaction rewrites history — incremental transcript readers
    /// compare it against the value they last saw to detect that
    /// already-fetched rows gained markers and a full reload is due.
    async fn supersede_watermark(&self, session_id: &SessionId) -> Result<Option<i64>>;

    /// `created_at` of every session created in `[from, to)`, in no
    /// particular order. A flat-column projection for creation-rate
    /// analytics — no session blobs are decoded.
    async fn session_created_times(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<DateTime<Utc>>>;

    /// Conversations the dream pass has not consolidated yet, newest
    /// activity first. Two disjoint arms, split on whether the session has
    /// ever been offered to a pass (`sessions.dreamed_through_ordinal`):
    ///
    /// - **Never offered** → sessions carrying a human-authored message in
    ///   `[since, until)`. "A person said something in this window" is what
    ///   keeps the pass from feeding on itself: a dream fire, and a silent
    ///   cron fire, contain no human message at all, and a subagent's prompt
    ///   is `MessageSource::Agent`. It is also what bounds the first-ever
    ///   pass, which would otherwise be handed all of history.
    /// - **Offered before** → every row above the cursor, **whoever wrote
    ///   it**. Dropping the human-message requirement here is the whole
    ///   point: the rows a pass misses are the ones appended *after* it read
    ///   — a turn that was still running, or a background-notification
    ///   delivery, which opens no turn at all and lands
    ///   `MessageSource::Agent` rows hours later. No predicate over human
    ///   messages in any window will ever select those, so a session whose
    ///   tail arrived late would stay unread forever.
    ///
    /// Compaction's own rows are excluded from both arms: they are copies of
    /// material already present (the originals are never deleted), so
    /// counting them would make a compaction look like activity and reading
    /// them would consolidate the same exchange twice.
    ///
    /// Not scoped by identity: a deployment today serves a single human, and
    /// `user.id` could not partition them anyway — one human routinely holds
    /// several ids, one per code path that minted a session. See
    /// `docs/todo/user-identity.md`.
    async fn dream_candidates(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<DreamCandidate>>;

    /// Advance the dream pass's cursor for `session_id` to `ordinal`
    /// (max-wins, so a stale writer cannot rewind it). Returns `false` when
    /// the session row is gone.
    ///
    /// Called once a conversation has actually been **offered** to a pass —
    /// after its fire is dispatched, never before. Every earlier failure
    /// (mint, digest, store) therefore leaves the cursor where it was, and
    /// the next pass re-offers the conversation instead of losing it.
    async fn set_dreamed_through_ordinal(
        &self,
        session_id: &SessionId,
        ordinal: i64,
    ) -> Result<bool>;

    /// Newest human-authored active row (`source` `user` /
    /// `user_interjection`) for each requested session, one grouped
    /// query. Sessions without such a row yield no entry. Powers the
    /// chat list's first-line previews without a per-session fan-out.
    async fn last_user_messages(
        &self,
        session_ids: &[SessionId],
    ) -> Result<Vec<(SessionId, DateTime<Utc>, ChatMessage)>>;

    /// Last `limit` active rows of each requested session, ascending
    /// within each session, one grouped query. The batched sibling of
    /// [`Self::load_active_session_messages_tail`] for list surfaces.
    async fn active_tails(
        &self,
        session_ids: &[SessionId],
        limit: usize,
    ) -> Result<Vec<(SessionId, i64, DateTime<Utc>, ChatMessage)>>;

    /// Up to `limit` active rows per requested session strictly above
    /// that session's chat-list `read_cursor` (unset cursor reads as
    /// -1), ascending, one grouped query joined against the cursor in
    /// SQL. Powers the unread badge without a per-session
    /// cursor-read + scan pair.
    async fn unread_scan(
        &self,
        session_ids: &[SessionId],
        limit: usize,
    ) -> Result<Vec<(SessionId, ChatMessage)>>;

    /// `title` of each requested session (flat column, no blob
    /// decode). Sessions without a row yield no entry.
    async fn session_titles(
        &self,
        session_ids: &[SessionId],
    ) -> Result<Vec<(SessionId, Option<String>)>>;

    /// Channel tag of each requested session (flat column, no blob
    /// decode). Sessions without a row yield no entry. Powers batch
    /// scope checks without a per-id session load.
    async fn session_channels(&self, session_ids: &[SessionId])
    -> Result<Vec<(SessionId, String)>>;

    /// Bump `last_active` to `now` in one targeted write (column +
    /// embedded blob field together, atomically) — the per-message
    /// hot-path alternative to a full `get` + `save` round-trip pair.
    /// Returns `false` when the session id is unknown.
    async fn touch_last_active(&self, session_id: &SessionId, now: DateTime<Utc>) -> Result<bool>;

    /// Total number of stored sessions. Status surfaces need the
    /// number, not the rows.
    async fn count_sessions(&self) -> Result<usize>;

    /// 0-indexed position of the row with `ordinal == ordinal` within
    /// the session's active sequence (rows where `superseded_by IS
    /// NULL`, in ordinal order). Returns `None` when no active row has
    /// that ordinal — either compression has rewritten it away or it
    /// never existed.
    ///
    /// Cheaper than [`Self::load_session_messages_with_supersede`] +
    /// walking: backed by a `COUNT(*)` + `EXISTS` against the partial
    /// `idx_session_messages_active` index, so message contents never
    /// cross the wire. Used by paths that only need to translate a stored
    /// ordinal into its position in the active set.
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

    /// Reverse-paginated slice of the transcript for DISPLAY: at most
    /// `limit` rows whose `ordinal` is strictly below `before_ordinal`
    /// (or the tail when `before_ordinal` is `None`), in **ascending**
    /// ordinal order, each paired with its absolute ordinal and
    /// persisted `created_at` so the caller can page the next-older
    /// slice and render a per-message timestamp without a second lookup.
    ///
    /// Returns the **real conversation** — rows where
    /// `compaction_inserted = 0` — NOT the active set. So the superseded
    /// pre-compaction originals render (each real turn once) and the
    /// re-injected compaction copies are hidden. This is the display
    /// counterpart to [`Self::load_active_session_messages`], which
    /// returns the live LLM context (machinery included) and must never
    /// share this filter.
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

    /// Forward difference slice for DISPLAY: the next at most `limit`
    /// real-conversation rows (`compaction_inserted = 0`) whose `ordinal`
    /// is strictly **greater than** `after_ordinal`, in ascending order
    /// alongside each row's absolute ordinal and `created_at`. Powers the
    /// REST sync endpoint's cursor scan (and the push-preview read) — a
    /// live compaction's machinery never advances the thread; only genuine
    /// post-compaction turns do.
    async fn load_active_session_messages_since(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        limit: usize,
    ) -> Result<Vec<(i64, DateTime<Utc>, ChatMessage)>>;

    /// Compaction boundaries for the session: one `(ordinal, created_at)`
    /// per distinct `superseded_by` watermark — the summary-head row
    /// compaction wrote at that ordinal (the watermark value *is* that
    /// head's ordinal; see `apply_session_compaction`). Ascending by
    /// ordinal; empty when the session was never compacted. The chat view
    /// draws a "pre-compaction history" divider before the first displayed
    /// row at/after each boundary. Reads no transcript content.
    async fn compaction_boundaries(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<(i64, DateTime<Utc>)>>;

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
}
