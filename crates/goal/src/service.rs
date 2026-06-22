//! [`GoalService`] — the CRUD + status-transition facade over an
//! `aura_store::GoalStore` that both the goal tools and the `aura-agent`
//! continuation runtime drive. It is the single home of the "one current goal
//! per session" policy and the read-modify-write usage accounting, so neither
//! the tools nor the actor duplicate that logic.

use std::sync::Arc;

use aura_model::{Goal, GoalId, GoalStatus, SessionId};
use aura_store::goal::{GoalPatch, GoalStore};
use chrono::Utc;
use thiserror::Error;

/// Errors a goal operation can surface to its caller (a tool or the actor).
#[derive(Debug, Error)]
pub enum GoalError {
    /// `create` refused because the session already has an unfinished goal
    /// (any status other than `Complete`). One current goal per session.
    #[error("a goal is already active for this session: {objective}")]
    AlreadyActive { objective: String },
    #[error("no goal is set for this session")]
    NotFound,
    #[error("goal store error: {0}")]
    Store(String),
}

type Result<T> = std::result::Result<T, GoalError>;

/// Outcome of [`GoalService::pause`]. The transition guard ("pause is valid only
/// from `Active`") lives here so every caller — the `/goal pause` command and
/// the operator REST control — shares one rule instead of re-deriving it.
pub enum PauseOutcome {
    /// Was `Active`; now `Paused`.
    Paused(Goal),
    /// A goal exists but wasn't `Active` (terminal or already stopped); unchanged.
    NotActive(Goal),
    /// No goal is set.
    NoGoal,
}

/// Outcome of [`GoalService::resume`] — the mirror guard of [`PauseOutcome`].
pub enum ResumeOutcome {
    /// Was resumable (a stopped, non-terminal state); now `Active`.
    Resumed(Goal),
    /// Already `Active`; unchanged.
    AlreadyActive(Goal),
    /// `Complete` — nothing to resume; unchanged.
    Terminal(Goal),
    /// No goal is set.
    NoGoal,
}

/// CRUD + transitions over one session's current goal.
pub struct GoalService {
    store: Arc<dyn GoalStore>,
}

impl GoalService {
    pub fn new(store: Arc<dyn GoalStore>) -> Self {
        Self { store }
    }

    /// The session's current goal, or `None`.
    pub async fn current(&self, session_id: &SessionId) -> Result<Option<Goal>> {
        self.store
            .get(session_id)
            .await
            .map_err(|e| GoalError::Store(e.to_string()))
    }

    /// Start a new goal, enforcing the "one current goal" rule: fails with
    /// [`GoalError::AlreadyActive`] when an unfinished (non-`Complete`) goal
    /// exists. A terminal (`Complete`) goal is replaced. Returns the new goal,
    /// `Active`.
    pub async fn create(
        &self,
        session_id: &SessionId,
        objective: &str,
        token_budget: Option<u64>,
    ) -> Result<Goal> {
        if let Some(existing) = self.current(session_id).await?
            && !existing.status.is_terminal()
        {
            return Err(GoalError::AlreadyActive {
                objective: existing.objective,
            });
        }
        self.replace(session_id, objective, token_budget).await
    }

    /// Unconditionally install a fresh `Active` goal, replacing any existing row
    /// (used after the policy check, and to restart a terminal goal). Resets
    /// usage counters.
    pub async fn replace(
        &self,
        session_id: &SessionId,
        objective: &str,
        token_budget: Option<u64>,
    ) -> Result<Goal> {
        let now = Utc::now();
        let goal = Goal {
            id: GoalId::new(),
            objective: objective.to_string(),
            status: GoalStatus::Active,
            token_budget,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: now,
            updated_at: now,
        };
        self.store
            .upsert(session_id, &goal)
            .await
            .map_err(|e| GoalError::Store(e.to_string()))?;
        Ok(goal)
    }

    /// Flip the goal's status unconditionally. `Ok(false)` when no goal is set.
    /// The single-site model-driven (`update_goal` → `Complete`/`Blocked`) and
    /// system spend-stop (`BudgetLimited`/`SpendCapped`) transitions use this;
    /// the guarded user transitions go through [`Self::pause`]/[`Self::resume`].
    pub async fn set_status(&self, session_id: &SessionId, status: GoalStatus) -> Result<bool> {
        self.store
            .update(
                session_id,
                &GoalPatch {
                    status: Some(status),
                    ..GoalPatch::new()
                },
            )
            .await
            .map_err(|e| GoalError::Store(e.to_string()))
    }

    /// Pause an `Active` goal so the continuation loop stops at the next turn
    /// boundary. A no-op for any non-`Active` goal (or none).
    pub async fn pause(&self, session_id: &SessionId) -> Result<PauseOutcome> {
        let Some(goal) = self.current(session_id).await? else {
            return Ok(PauseOutcome::NoGoal);
        };
        if !goal.status.is_active() {
            return Ok(PauseOutcome::NotActive(goal));
        }
        self.set_status(session_id, GoalStatus::Paused).await?;
        Ok(match self.current(session_id).await? {
            Some(updated) => PauseOutcome::Paused(updated),
            None => PauseOutcome::NoGoal,
        })
    }

    /// Re-activate a stopped, non-terminal goal so the continuation loop resumes.
    /// A no-op for an `Active` or `Complete` goal (or none).
    pub async fn resume(&self, session_id: &SessionId) -> Result<ResumeOutcome> {
        let Some(goal) = self.current(session_id).await? else {
            return Ok(ResumeOutcome::NoGoal);
        };
        if goal.status.is_active() {
            return Ok(ResumeOutcome::AlreadyActive(goal));
        }
        if goal.status.is_terminal() {
            return Ok(ResumeOutcome::Terminal(goal));
        }
        self.set_status(session_id, GoalStatus::Active).await?;
        Ok(match self.current(session_id).await? {
            Some(updated) => ResumeOutcome::Resumed(updated),
            None => ResumeOutcome::NoGoal,
        })
    }

    /// Edit an existing goal in place (the `/goal <new objective> [--budget N]`
    /// path): always updates the objective, and raises/sets the per-goal token
    /// budget when `budget` is `Some` (a `None` budget leaves the existing cap
    /// untouched, so editing the objective alone never silently drops it).
    /// `Ok(false)` when no goal is set.
    pub async fn edit(
        &self,
        session_id: &SessionId,
        objective: &str,
        budget: Option<u64>,
    ) -> Result<bool> {
        self.store
            .update(
                session_id,
                &GoalPatch {
                    objective: Some(objective.to_string()),
                    token_budget: budget.map(Some),
                    ..GoalPatch::new()
                },
            )
            .await
            .map_err(|e| GoalError::Store(e.to_string()))
    }

    /// Accrue a goal turn's token + wall-clock usage (read-modify-write; the
    /// per-session actor is single-threaded, so no lost-update race). Returns
    /// the updated goal, or `None` when no goal is set.
    pub async fn add_usage(
        &self,
        session_id: &SessionId,
        tokens: u64,
        seconds: u64,
    ) -> Result<Option<Goal>> {
        let Some(goal) = self.current(session_id).await? else {
            return Ok(None);
        };
        let patch = GoalPatch {
            tokens_used: Some(goal.tokens_used.saturating_add(tokens)),
            time_used_seconds: Some(goal.time_used_seconds.saturating_add(seconds)),
            ..GoalPatch::new()
        };
        self.store
            .update(session_id, &patch)
            .await
            .map_err(|e| GoalError::Store(e.to_string()))?;
        self.current(session_id).await
    }

    /// Terminal delete — the one explicit per-row `DELETE` (`/goal clear`).
    /// `Ok(false)` when nothing was deleted.
    pub async fn clear(&self, session_id: &SessionId) -> Result<bool> {
        self.store
            .delete(session_id)
            .await
            .map_err(|e| GoalError::Store(e.to_string()))
    }

    /// Every session's current goal — the cross-session dashboard feed.
    pub async fn list_all(&self) -> Result<Vec<(SessionId, Goal)>> {
        self.store
            .list_all()
            .await
            .map_err(|e| GoalError::Store(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryGoalStore;

    fn service() -> GoalService {
        GoalService::new(Arc::new(MemoryGoalStore::new()))
    }

    #[tokio::test]
    async fn create_then_current_round_trips() {
        let svc = service();
        let sid = SessionId::from("s1");
        let g = svc.create(&sid, "ship it", Some(1000)).await.unwrap();
        assert_eq!(g.status, GoalStatus::Active);
        let got = svc.current(&sid).await.unwrap().unwrap();
        assert_eq!(got, g);
    }

    #[tokio::test]
    async fn create_fails_when_unfinished_goal_exists() {
        let svc = service();
        let sid = SessionId::from("s1");
        svc.create(&sid, "first", None).await.unwrap();
        let err = svc.create(&sid, "second", None).await.unwrap_err();
        assert!(matches!(err, GoalError::AlreadyActive { .. }));
    }

    #[tokio::test]
    async fn create_replaces_terminal_goal() {
        let svc = service();
        let sid = SessionId::from("s1");
        svc.create(&sid, "first", None).await.unwrap();
        assert!(svc.set_status(&sid, GoalStatus::Complete).await.unwrap());
        let g = svc.create(&sid, "second", None).await.unwrap();
        assert_eq!(g.objective, "second");
        assert_eq!(g.status, GoalStatus::Active);
    }

    #[tokio::test]
    async fn create_blocked_goal_blocks_new_create() {
        let svc = service();
        let sid = SessionId::from("s1");
        svc.create(&sid, "first", None).await.unwrap();
        svc.set_status(&sid, GoalStatus::Blocked).await.unwrap();
        // Blocked is unfinished (resumable), so create still refuses.
        assert!(matches!(
            svc.create(&sid, "second", None).await.unwrap_err(),
            GoalError::AlreadyActive { .. }
        ));
    }

    #[tokio::test]
    async fn add_usage_accumulates() {
        let svc = service();
        let sid = SessionId::from("s1");
        svc.create(&sid, "x", None).await.unwrap();
        svc.add_usage(&sid, 100, 5).await.unwrap();
        let g = svc.add_usage(&sid, 50, 3).await.unwrap().unwrap();
        assert_eq!(g.tokens_used, 150);
        assert_eq!(g.time_used_seconds, 8);
    }

    #[tokio::test]
    async fn edit_and_clear() {
        let svc = service();
        let sid = SessionId::from("s1");
        svc.create(&sid, "old", None).await.unwrap();
        assert!(svc.edit(&sid, "new", None).await.unwrap());
        assert_eq!(svc.current(&sid).await.unwrap().unwrap().objective, "new");
        assert!(svc.clear(&sid).await.unwrap());
        assert!(svc.current(&sid).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn edit_raises_budget_but_leaves_it_untouched_when_omitted() {
        let svc = service();
        let sid = SessionId::from("s1");
        svc.create(&sid, "obj", Some(1_000)).await.unwrap();
        svc.edit(&sid, "obj2", None).await.unwrap();
        let g = svc.current(&sid).await.unwrap().unwrap();
        assert_eq!(g.objective, "obj2");
        assert_eq!(g.token_budget, Some(1_000));
        // The post-BudgetLimited raise-the-cap path.
        svc.edit(&sid, "obj2", Some(5_000)).await.unwrap();
        assert_eq!(
            svc.current(&sid).await.unwrap().unwrap().token_budget,
            Some(5_000)
        );
    }

    #[tokio::test]
    async fn add_usage_without_goal_is_none() {
        let svc = service();
        assert!(
            svc.add_usage(&SessionId::from("nope"), 1, 1)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn pause_then_resume_round_trips_active() {
        let svc = service();
        let sid = SessionId::from("s1");
        svc.create(&sid, "x", None).await.unwrap();
        assert!(matches!(
            svc.pause(&sid).await.unwrap(),
            PauseOutcome::Paused(g) if g.status == GoalStatus::Paused
        ));
        assert!(matches!(
            svc.resume(&sid).await.unwrap(),
            ResumeOutcome::Resumed(g) if g.status == GoalStatus::Active
        ));
    }

    #[tokio::test]
    async fn pause_is_a_no_op_off_active() {
        let svc = service();
        let sid = SessionId::from("s1");
        assert!(matches!(
            svc.pause(&sid).await.unwrap(),
            PauseOutcome::NoGoal
        ));
        svc.create(&sid, "x", None).await.unwrap();
        svc.set_status(&sid, GoalStatus::Complete).await.unwrap();
        assert!(matches!(
            svc.pause(&sid).await.unwrap(),
            PauseOutcome::NotActive(g) if g.status == GoalStatus::Complete
        ));
    }

    #[tokio::test]
    async fn resume_rejects_active_and_terminal() {
        let svc = service();
        let sid = SessionId::from("s1");
        assert!(matches!(
            svc.resume(&sid).await.unwrap(),
            ResumeOutcome::NoGoal
        ));
        svc.create(&sid, "x", None).await.unwrap();
        assert!(matches!(
            svc.resume(&sid).await.unwrap(),
            ResumeOutcome::AlreadyActive(_)
        ));
        svc.set_status(&sid, GoalStatus::Complete).await.unwrap();
        assert!(matches!(
            svc.resume(&sid).await.unwrap(),
            ResumeOutcome::Terminal(_)
        ));
    }
}
