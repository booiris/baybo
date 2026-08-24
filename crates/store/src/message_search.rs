//! Full-text search over transcript prose. See `docs/search.md`.

use async_trait::async_trait;
use baybo_model::{ChannelType, ChatMessage, SessionId};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// One matching transcript row, resolved back to its message.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub session_id: SessionId,
    pub ordinal: i64,
    /// The channel of the session holding this row, snapshotted at index time.
    /// `subagent` marks a row from a subagent's own run rather than from the
    /// conversation that spawned it.
    pub channel: ChannelType,
    pub created_at: DateTime<Utc>,
    /// `Some(n)` when compaction stamped this row — `n` is the ordinal where the
    /// compaction's re-inserted rows begin, and every row active at that moment
    /// points at the same one.
    ///
    /// It is NOT a "this is not on screen" marker, and callers must not navigate
    /// to `n`. The display read filters `compaction_inserted = 0`, not
    /// `superseded_by IS NULL` (see `load_active_session_messages_tail`), so the
    /// superseded ORIGINAL still renders and `ordinal` is the address to jump to;
    /// `n` names a re-injected machinery row, which the display read excludes and
    /// which therefore can never be on screen. What this actually says is that
    /// the model's context was rewritten after this row — a fact about the LLM's
    /// window, not about what the user can see.
    pub superseded_by: Option<i64>,
    /// The original message, not the segmented index text. Highlight by
    /// substring: a phrase of unigrams matches exactly the substring the user
    /// typed, so a client-side match agrees with what the index matched.
    pub message: ChatMessage,
    /// FTS5 bm25 score. More negative is a better match; hits arrive sorted.
    pub score: f64,
}

/// Which sessions a search may reach.
///
/// Every session's prose is indexed — a chat, a cron fire, a subagent's own run —
/// so scope is the caller's to state, and the default states the narrow one. A
/// user-facing search box wants `channel: Some(owner)` and every flag false;
/// that is the same scope the chat list renders, and anything wider returns rows
/// the box cannot open or the user asked to lose.
#[derive(Debug, Clone, Default)]
pub struct SearchScope {
    /// `None` reaches every channel, including a subagent's own run
    /// (`SUBAGENT_CHANNEL_TAG`) and every bot channel.
    pub channel: Option<ChannelType>,
    /// Restrict to one conversation — "find it in *this* chat". `None` searches
    /// across all of them.
    ///
    /// Composes with, rather than overrides, the other filters: a session that
    /// fails them still matches nothing. Naming a hidden session therefore needs
    /// `include_hidden` too, which is the honest reading — the caller asked for
    /// a session, not for the hidden rule to lapse.
    pub session: Option<SessionId>,
    /// `hidden` is the user's "remove this from my list". A quarter of a real
    /// index sits in hidden sessions, so defaulting this on would quietly
    /// resurface what they asked to lose.
    pub include_hidden: bool,
    pub include_archived: bool,
    /// Reach cron fire sessions that are **not** conversations of their own: a
    /// one-shot's private workspace (its result is reported into the
    /// conversation that scheduled it), and every historical fire from before
    /// recurring fires became conversations.
    ///
    /// Off by default because such a session is unreachable by construction on
    /// the other side: the chat list drops it (`is_private_cron_session`) and the
    /// REST attach path 404s it, so a hit there names a conversation no client
    /// can list. A recurring fire IS a conversation and is never affected by
    /// this flag.
    pub include_cron_workspaces: bool,
}

/// Read-only search over transcript prose.
///
/// Implementors own the query dialect: callers pass the user's raw string and
/// never build FTS5 syntax themselves — `-`, `*`, `^`, `:`, `NEAR` and `OR` all
/// mean something to FTS5 bare, so an unescaped query is both a syntax error and
/// an injection surface.
#[async_trait]
pub trait MessageSearchStore: Send + Sync {
    /// Rows within `scope` whose prose matches `query`, best first, capped at
    /// `limit`.
    ///
    /// A query with nothing indexable in it (punctuation only, or empty) yields
    /// an empty result rather than an error — it is a user typing, not a bug.
    async fn search_messages(
        &self,
        query: &str,
        scope: &SearchScope,
        limit: u32,
    ) -> Result<Vec<SearchHit>>;
}
