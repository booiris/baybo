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

/// Wall-clock bounds and liveness for one session's turns.
#[derive(Debug, Clone)]
pub struct SessionTurnBounds {
    pub session_id: SessionId,
    /// Earliest `started_at`, falling back to `created_at` for a turn that was
    /// queued but never started.
    pub first_started_at: DateTime<Utc>,
    /// Latest `ended_at`. `None` while any turn is still open, which is what
    /// makes "still running" and "ran for N" one lookup rather than two.
    pub last_ended_at: Option<DateTime<Utc>>,
    /// At least one turn has not settled.
    ///
    /// Read off `ended_at` rather than `status_kind`: `Turn::transition`
    /// stamps `ended_at` if and only if the target status is terminal, so the
    /// timestamp already carries the answer and this layer — which owns none
    /// of `baybo-turn`'s vocabulary — needs no third copy of the
    /// pending/in_progress/stuck spelling.
    pub live: bool,
    /// `status_kind` of the newest turn by `created_at`, same encoding as
    /// [`TurnRow::status_kind`].
    pub latest_status_kind: String,
}

impl SessionTurnBounds {
    /// Fold a session's turn rows. `None` when the session has no turns —
    /// a spawned child whose actor has not opened one yet.
    pub fn fold(session_id: &SessionId, rows: &[TurnRow]) -> Option<Self> {
        let first_started_at = rows
            .iter()
            .map(|r| r.started_at.unwrap_or(r.created_at))
            .min()?;
        let live = rows.iter().any(|r| r.ended_at.is_none());
        // An open turn leaves the whole session open — a max over just the
        // closed ones would report a duration that quietly stopped growing
        // while the child was still working.
        let last_ended_at = if live {
            None
        } else {
            rows.iter().filter_map(|r| r.ended_at).max()
        };
        let latest_status_kind = rows
            .iter()
            .max_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)))
            .map(|r| r.status_kind.clone())?;
        Some(Self {
            session_id: session_id.clone(),
            first_started_at,
            last_ended_at,
            live,
            latest_status_kind,
        })
    }
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
    /// Turn bounds for a BOUNDED list of sessions, one grouped query.
    ///
    /// [`Self::session_turn_stats`] answers a similar question but groups the
    /// whole table, and [`Self::list_active_by_session`] is per-session. The
    /// subagent list surface needs both liveness and wall-clock for up to a
    /// hundred children and is polled while its sheet is open, so neither
    /// shape works: one is a full scan per poll, the other is a fan-out.
    ///
    /// Sessions with no turns are simply absent from the result. The default
    /// implementation is the naive fan-out so in-memory fakes need no update;
    /// the sqlite backend overrides it.
    async fn session_turn_bounds(
        &self,
        session_ids: &[SessionId],
    ) -> Result<Vec<SessionTurnBounds>> {
        let mut out = Vec::new();
        for session_id in session_ids {
            let rows = self.list_by_session(session_id).await?;
            if let Some(bounds) = SessionTurnBounds::fold(session_id, &rows) {
                out.push(bounds);
            }
        }
        Ok(out)
    }
    /// Number of turns with the given snake_case `TurnStatusKind` wire
    /// string. Status surfaces need the number, not the rows.
    async fn count_by_status_kind(&self, status_kind: &str) -> Result<usize>;
    /// Turns whose status is non-terminal (`pending` / `in_progress` /
    /// `stuck`). Used by admin queries surfacing turns needing attention.
    async fn list_recoverable(&self) -> Result<Vec<TurnRow>>;
}
