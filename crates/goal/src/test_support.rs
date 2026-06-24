//! In-memory [`GoalStore`] for tests — the unit tests here and downstream
//! integration tests (via `features = ["test-support"]`) use it so they don't
//! drag in the libsql adapter. Gated so it never ships in release.

use std::collections::HashMap;

use async_trait::async_trait;
use baybo_model::{Goal, SessionId};
use baybo_store::goal::{GoalPatch, GoalStore, Result};
use parking_lot::Mutex;

/// Maps each session to its single current goal. Mirrors `LibsqlGoalStore`
/// semantics: `upsert` replaces, `update` is a sparse patch, `delete` removes.
#[derive(Default)]
pub struct MemoryGoalStore {
    by_session: Mutex<HashMap<SessionId, Goal>>,
}

impl MemoryGoalStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GoalStore for MemoryGoalStore {
    async fn get(&self, session_id: &SessionId) -> Result<Option<Goal>> {
        Ok(self.by_session.lock().get(session_id).cloned())
    }

    async fn upsert(&self, session_id: &SessionId, goal: &Goal) -> Result<()> {
        self.by_session
            .lock()
            .insert(session_id.clone(), goal.clone());
        Ok(())
    }

    async fn update(&self, session_id: &SessionId, patch: &GoalPatch) -> Result<bool> {
        let mut guard = self.by_session.lock();
        let Some(goal) = guard.get_mut(session_id) else {
            return Ok(false);
        };
        if let Some(status) = patch.status {
            goal.status = status;
        }
        if let Some(objective) = &patch.objective {
            goal.objective = objective.clone();
        }
        if let Some(token_budget) = patch.token_budget {
            goal.token_budget = token_budget;
        }
        if let Some(tokens_used) = patch.tokens_used {
            goal.tokens_used = tokens_used;
        }
        if let Some(time_used_seconds) = patch.time_used_seconds {
            goal.time_used_seconds = time_used_seconds;
        }
        goal.updated_at = chrono::Utc::now();
        Ok(true)
    }

    async fn delete(&self, session_id: &SessionId) -> Result<bool> {
        Ok(self.by_session.lock().remove(session_id).is_some())
    }

    async fn list_all(&self) -> Result<Vec<(SessionId, Goal)>> {
        let guard = self.by_session.lock();
        let mut out: Vec<(SessionId, Goal)> =
            guard.iter().map(|(s, g)| (s.clone(), g.clone())).collect();
        out.sort_by(|a, b| {
            b.1.updated_at
                .cmp(&a.1.updated_at)
                .then(a.1.id.cmp(&b.1.id))
        });
        Ok(out)
    }
}
