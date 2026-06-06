//! In-memory [`TaskStore`] for tests — the tool unit tests here and
//! downstream integration tests (via `features = ["test-support"]`) use it so
//! they don't drag in the libsql adapter. Gated so it never ships in release.

use std::collections::HashMap;

use async_trait::async_trait;
use aura_model::{SessionId, Task, TaskId};
use aura_store::task::{Result, TaskPatch, TaskStore};
use parking_lot::Mutex;

/// Maps each session to its checklist. Mirrors `LibsqlTaskStore` semantics:
/// per-row updates, idempotent `create` on id.
#[derive(Default)]
pub struct MemoryTaskStore {
    by_session: Mutex<HashMap<SessionId, Vec<Task>>>,
}

impl MemoryTaskStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TaskStore for MemoryTaskStore {
    async fn create(&self, session_id: &SessionId, task: &Task) -> Result<()> {
        let mut guard = self.by_session.lock();
        let list = guard.entry(session_id.clone()).or_default();
        match list.iter_mut().find(|t| t.id == task.id) {
            Some(existing) => *existing = task.clone(),
            None => list.push(task.clone()),
        }
        Ok(())
    }

    async fn get(&self, session_id: &SessionId, task_id: &TaskId) -> Result<Option<Task>> {
        let guard = self.by_session.lock();
        Ok(guard
            .get(session_id)
            .and_then(|list| list.iter().find(|t| &t.id == task_id).cloned()))
    }

    async fn list(&self, session_id: &SessionId) -> Result<Vec<Task>> {
        let guard = self.by_session.lock();
        let mut list = guard.get(session_id).cloned().unwrap_or_default();
        list.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(list)
    }

    async fn update(&self, session_id: &SessionId, patch: &TaskPatch) -> Result<bool> {
        let mut guard = self.by_session.lock();
        let Some(list) = guard.get_mut(session_id) else {
            return Ok(false);
        };
        let Some(task) = list.iter_mut().find(|t| t.id == patch.task_id) else {
            return Ok(false);
        };
        if let Some(status) = patch.status {
            task.status = status;
        }
        if let Some(subject) = &patch.subject {
            task.subject = subject.clone();
        }
        if let Some(description) = &patch.description {
            task.description = description.clone();
        }
        if let Some(depends_on) = &patch.depends_on {
            task.depends_on = depends_on.clone();
        }
        task.updated_at = chrono::Utc::now();
        Ok(true)
    }

    async fn delete(&self, session_id: &SessionId, task_id: &TaskId) -> Result<bool> {
        let mut guard = self.by_session.lock();
        let Some(list) = guard.get_mut(session_id) else {
            return Ok(false);
        };
        let before = list.len();
        list.retain(|t| &t.id != task_id);
        Ok(list.len() != before)
    }
}
