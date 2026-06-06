//! Persistence interface for the per-session planning checklist
//! (`session_tasks` table) behind the `Task*` tool family.
//!
//! The checklist is a live, mutable collection: each `TaskUpdate` touches a
//! single row, concurrently with the `sessions`-row writers (`touch`, the
//! actor's full-blob `save`). A dedicated row-keyed table gives the same
//! anti-clobber property the `sessions.hidden` / `sessions.last_llm` flat
//! columns buy, but at row granularity — two updates to different tasks, or
//! a `touch` racing a task mutation, never collide. Rows are reaped by
//! `ON DELETE CASCADE` from `sessions`; the runtime never sweeps them.

use async_trait::async_trait;
use aura_model::{SessionId, Task, TaskId, TaskStatus};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Sparse update for `TaskUpdate`. A `None` field is left untouched, so two
/// concurrent updates to different attributes of one task don't stomp each
/// other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPatch {
    pub task_id: TaskId,
    pub status: Option<TaskStatus>,
    pub subject: Option<String>,
    pub description: Option<String>,
    pub depends_on: Option<Vec<TaskId>>,
}

impl TaskPatch {
    /// A patch that touches nothing but identifies the target task.
    pub fn new(task_id: TaskId) -> Self {
        Self {
            task_id,
            status: None,
            subject: None,
            description: None,
            depends_on: None,
        }
    }

    /// `true` when no field would change — the tool layer rejects an empty
    /// patch rather than issuing a no-op `UPDATE`.
    pub fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.subject.is_none()
            && self.description.is_none()
            && self.depends_on.is_none()
    }
}

/// Per-session planning-checklist persistence. All mutations are scoped by
/// `session_id`; the value type ([`Task`]) does not carry it.
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// Insert one task. Idempotent on `(session_id, task.id)` — the caller
    /// mints the [`TaskId`], so a retry is a no-op upsert rather than a
    /// duplicate.
    async fn create(&self, session_id: &SessionId, task: &Task) -> Result<()>;

    /// Read one task. `Ok(None)` when the id is absent for this session.
    async fn get(&self, session_id: &SessionId, task_id: &TaskId) -> Result<Option<Task>>;

    /// Every task for `session_id`, ordered by `created_at` for a stable
    /// list view. A row whose stored status is unrecognized (written by a
    /// future variant) is skipped rather than failing the whole call.
    async fn list(&self, session_id: &SessionId) -> Result<Vec<Task>>;

    /// Apply a sparse patch to one task. Per-row `UPDATE` touching only the
    /// changed columns — clobber-isolated against `touch` and against
    /// concurrent updates to other tasks. `Ok(false)` when the id is absent.
    async fn update(&self, session_id: &SessionId, patch: &TaskPatch) -> Result<bool>;

    /// Hard-delete one task (plain `DELETE`, no tombstone). `Ok(false)` when
    /// the id was already absent. Whole-session removal happens via the
    /// `sessions` cascade, not here.
    async fn delete(&self, session_id: &SessionId, task_id: &TaskId) -> Result<bool>;
}
