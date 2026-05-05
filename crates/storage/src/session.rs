use async_trait::async_trait;
use aura_model::{LineageKind, Session, SessionId};
use chrono::{DateTime, Utc};

use crate::error::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Persistence interface for sessions.
///
/// `SessionId` is the caller-supplied opaque string (see
/// `aura_model::SessionId`). Lineage / fork relationships are stored
/// inline on the session row (`root_session_id`, `lineage_*` columns)
/// — there is no separate forks table; fork reads are a view-layer
/// UNION over the source session's prefix and the new session's own
/// jobs.
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get(&self, session_id: &SessionId) -> Result<Option<Session>>;
    async fn save(&self, session: &Session) -> Result<()>;
    /// Hard-delete the session.
    ///
    /// Returns `Ok(true)` if the row existed and was removed, `Ok(false)`
    /// if it did not exist (idempotent).
    /// Returns `Err(StorageError::HasLiveForks { .. })` if any session has a
    /// `LineageKind::UserFork` pointing into `session_id`. The live-fork
    /// scan and the parent-row delete run inside one `BEGIN IMMEDIATE`
    /// write transaction — a fork inserted concurrently either lands
    /// before the scan (and is reported back) or after the commit (and
    /// protects a still-live parent on the *next* call).
    ///
    /// Does **not** drain in-flight subagents — that is the
    /// `SessionManager`'s responsibility (cancel propagation through
    /// the actor token tree happens before this call).
    async fn delete(&self, session_id: &SessionId) -> Result<bool>;
    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<SessionId>>;
    /// Return every live session, ordered by `last_active` descending.
    /// Operator-facing: drives `aura session list`.
    async fn list_all(&self) -> Result<Vec<Session>>;
    /// Return the live forks (sessions with `LineageKind::UserFork`)
    /// whose `parent_session_id` equals the given session.
    async fn list_live_forks(&self, source_session_id: &SessionId) -> Result<Vec<SessionId>>;

    /// Return every immediate live descendant (Subagent or UserFork)
    /// of the given parent. Powers `lineage_tree` and
    /// `list_active_subagents`. Order is unspecified.
    async fn list_lineage_children(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<Vec<(SessionId, LineageKind)>>;
}
