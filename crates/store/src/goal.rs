//! Persistence interface for a session's current autonomous objective
//! (`session_goals` table) behind the `/goal` continuation engine and the
//! goal tool family.
//!
//! One **current** goal per session — the table is keyed by `session_id`. The
//! row is mutated out-of-band of the turn (the model's `update_goal`, the
//! `/goal` command, the per-turn token/time accounting) concurrently with the
//! `sessions`-row writers (`touch`, the actor's full-blob `save`). A dedicated
//! row-keyed table gives the same anti-clobber property the `sessions.last_llm`
//! flat column buys, but for goal state. The row is reaped by `ON DELETE
//! CASCADE` from `sessions`; the only explicit `DELETE` is `/goal clear`. The
//! runtime never sweeps goals (session data is core data).

use async_trait::async_trait;
use aura_model::{Goal, GoalStatus, SessionId};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Sparse update for a goal. A `None` field is left untouched, so the model's
/// `update_goal` (status), the `/goal` command (objective/status), and the
/// per-turn accounting (tokens/time) never stomp each other's columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalPatch {
    pub status: Option<GoalStatus>,
    pub objective: Option<String>,
    /// Outer `None` = leave the budget untouched; `Some(None)` = clear it (run
    /// unbudgeted); `Some(Some(n))` = set the per-goal token budget to `n`. The
    /// nesting is what lets `/goal <obj> --budget N` raise the cap on an
    /// existing goal without an unconditional overwrite.
    pub token_budget: Option<Option<u64>>,
    pub tokens_used: Option<u64>,
    pub time_used_seconds: Option<u64>,
}

impl GoalPatch {
    /// A patch that touches nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when no field would change.
    pub fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.objective.is_none()
            && self.token_budget.is_none()
            && self.tokens_used.is_none()
            && self.time_used_seconds.is_none()
    }
}

/// Per-session goal persistence. All mutations are scoped by `session_id`; the
/// value type ([`Goal`]) does not carry it.
#[async_trait]
pub trait GoalStore: Send + Sync {
    /// The session's current goal, or `Ok(None)` when none is set.
    async fn get(&self, session_id: &SessionId) -> Result<Option<Goal>>;

    /// Insert or replace the session's current goal (one per session). The
    /// caller enforces the "fails if an unfinished goal exists" policy before
    /// calling — the store unconditionally replaces.
    async fn upsert(&self, session_id: &SessionId, goal: &Goal) -> Result<()>;

    /// Apply a sparse patch to the session's goal. Per-row `UPDATE` touching
    /// only the changed columns — clobber-isolated against the `sessions`
    /// writers. `Ok(false)` when no goal exists for the session.
    async fn update(&self, session_id: &SessionId, patch: &GoalPatch) -> Result<bool>;

    /// Hard-delete the session's goal (plain `DELETE`, no tombstone) — the one
    /// explicit per-row delete (`/goal clear`). `Ok(false)` when none existed.
    /// Whole-session removal happens via the `sessions` cascade, not here.
    async fn delete(&self, session_id: &SessionId) -> Result<bool>;

    /// Every session's current goal, newest first — the cross-session feed for
    /// the operator dashboard's goals column. A row whose stored status is
    /// unrecognized (written by a future variant) is skipped rather than
    /// failing the whole call.
    async fn list_all(&self) -> Result<Vec<(SessionId, Goal)>>;
}
