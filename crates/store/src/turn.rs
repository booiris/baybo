use async_trait::async_trait;
use baybo_model::{SessionId, TurnId};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Persistence row for a turn.
///
/// The full domain `Turn` (with its state machine and rich field types)
/// is serialized into `data`; the remaining fields are projected out so
/// the backend can index/filter without deserializing. `baybo-turn` owns
/// the `Turn` type and converts to/from this row at the persistence
/// boundary (`Turn::to_row` / `Turn::from_row`) — that keeps the turn state
/// machine out of this leaf crate while still letting `TurnStore` live
/// here alongside every other store contract.
#[derive(Debug, Clone)]
pub struct TurnRow {
    pub id: TurnId,
    pub session_id: SessionId,
    pub parent_turn_id: Option<TurnId>,
    /// The turn's input kind rendered as its wire string. Denormalised
    /// for display only — never filtered in SQL (`from_row` rebuilds the
    /// whole `Turn` from `data`).
    pub kind: String,
    /// `TurnStatusKind` rendered as its snake_case string — drives the
    /// `list_by_status_kind` / `list_recoverable` filters.
    pub status_kind: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    /// The serialized `Turn` aggregate. Opaque to this layer.
    pub data: String,
}

/// Per-session turn aggregates for list surfaces: how many turns a
/// session has and the `status_kind` of its newest turn (by
/// `created_at`). One grouped query replaces a `list_by_session`
/// fan-out across every session.
#[derive(Debug, Clone)]
pub struct SessionTurnStats {
    pub session_id: SessionId,
    pub turn_count: usize,
    /// `TurnStatusKind` of the latest turn, rendered as its snake_case
    /// wire string (same encoding as [`TurnRow::status_kind`]).
    pub latest_status_kind: String,
}

/// Persistence backend for turns. Trades in [`TurnRow`] rather than the
/// rich `baybo-turn` types so the contract stays in this leaf crate.
#[async_trait]
pub trait TurnStore: Send + Sync {
    async fn create(&self, turn: &TurnRow) -> Result<()>;
    async fn get(&self, turn_id: &TurnId) -> Result<Option<TurnRow>>;
    /// Persist the mutable state of a turn (status, timestamps, payload).
    async fn save(&self, turn: &TurnRow) -> Result<()>;
    async fn list_by_session(&self, session_id: &SessionId) -> Result<Vec<TurnRow>>;
    /// Non-terminal turns (`pending` / `in_progress` / `stuck`) for one session,
    /// scoped + status-filtered at the store. Lets callers (e.g. `/stop`) find
    /// a session's live turns without loading a long-lived session's entire
    /// turn history just to filter it down to the few in flight.
    async fn list_active_by_session(&self, session_id: &SessionId) -> Result<Vec<TurnRow>>;
    /// Every session that has at least one non-terminal turn, as one
    /// grouped query rather than a [`Self::list_active_by_session`] per
    /// candidate.
    ///
    /// The dream pass asks this to leave a conversation alone while its
    /// turn is still writing: read now and it consolidates half an
    /// exchange, and the rows the turn appends afterwards carry
    /// `MessageSource::Agent`, so nothing that selects on human messages
    /// would ever offer them again. Deferring is only safe because the
    /// pass's cursor is an ordinal it did not advance — see
    /// [`crate::session::SessionStore::set_dreamed_through_ordinal`].
    async fn sessions_with_live_turns(&self) -> Result<Vec<SessionId>>;
    /// Filter by the snake_case `TurnStatusKind` wire string.
    async fn list_by_status_kind(&self, status_kind: &str) -> Result<Vec<TurnRow>>;
    async fn list_children(&self, parent_turn_id: &TurnId) -> Result<Vec<TurnRow>>;
    /// Return every stored turn. Ordering unspecified — callers sort.
    async fn list_all(&self) -> Result<Vec<TurnRow>>;
    /// One page of turns, newest `created_at` first, optionally
    /// filtered by the snake_case `TurnStatusKind` wire string, plus
    /// the total matching count. Ordering + paging live in SQL so a
    /// page never materialises the full table.
    async fn list_page(
        &self,
        status_kind: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<TurnRow>, usize)>;
    /// Turn aggregates grouped by session — one [`SessionTurnStats`] per
    /// session that has at least one turn. Ordering unspecified.
    async fn session_turn_stats(&self) -> Result<Vec<SessionTurnStats>>;
    /// Number of turns with the given snake_case `TurnStatusKind` wire
    /// string. Status surfaces need the number, not the rows.
    async fn count_by_status_kind(&self, status_kind: &str) -> Result<usize>;
    /// Turns whose status is non-terminal (`pending` / `in_progress` /
    /// `stuck`). Used by admin queries surfacing turns needing attention.
    async fn list_recoverable(&self) -> Result<Vec<TurnRow>>;
}
