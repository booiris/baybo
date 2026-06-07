//! The session planning checklist: the durable value types behind the
//! `Task*` tool family.
//!
//! Pure data, shared one-way (mirrors [`crate::spawn_protocol::PendingBackgroundResult`]):
//! `aura-task` (the tool boundary) and `aura-context` (the per-turn
//! re-injection renderer) both reach for these. Persistence lives in the
//! dedicated `session_tasks` table behind `aura_store::TaskStore`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::TaskId;

/// Tool names for the `Task*` family. Defined here (next to the value types) so
/// the agent loop can recognize a checklist mutation without depending on the
/// `aura-task` crate — mirroring `SPAWN_SUBAGENT_TOOL_NAME`.
pub const TASK_CREATE_TOOL_NAME: &str = "TaskCreate";
pub const TASK_GET_TOOL_NAME: &str = "TaskGet";
pub const TASK_LIST_TOOL_NAME: &str = "TaskList";
pub const TASK_UPDATE_TOOL_NAME: &str = "TaskUpdate";

/// The subset of `Task*` tools whose execution mutates the checklist, so the
/// agent loop knows to refresh the per-turn reminder (and re-emit the web
/// surface) afterward. `TaskList` / `TaskGet` are read-only and excluded.
pub const TASK_MUTATING_TOOL_NAMES: &[&str] = &[TASK_CREATE_TOOL_NAME, TASK_UPDATE_TOOL_NAME];

/// Lifecycle of one checklist entry: `pending -> in_progress -> completed`.
/// Abandoning a task is a *deletion* (`TaskUpdate(status: "deleted")`), not a
/// stored status — matching Claude Code's three-state model.
///
/// The "at most one `InProgress` at a time" convention is prompt guidance
/// surfaced in the tool descriptions, not a store invariant — matching
/// Claude Code's `Task*` and Codex's `update_plan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    /// Every variant, in lifecycle order — the single source of truth for the
    /// `enum` list in the tool parameter schemas.
    pub const ALL: [TaskStatus; 3] = [Self::Pending, Self::InProgress, Self::Completed];

    /// Canonical wire/storage string — the single source of truth for the
    /// `session_tasks.status` column and the checklist renderer. Kept in
    /// lockstep with [`Self::from_storage_str`] (and the `snake_case` serde
    /// rename) right here so the spelling can't drift across the boundary.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    /// Parse the stored column back. `None` for an unknown value so a row
    /// written by a future variant is skipped rather than failing the whole
    /// `TaskStore::list`.
    pub fn from_storage_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

/// One entry in a session's planning checklist.
///
/// The session is the top of one trace tree; its task list is scoped by
/// `session_id` in storage (not carried on the value). No read/acknowledged
/// flag — unread state is client-only (see `docs/modules/storage.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    /// Brief imperative title, e.g. "Add the TaskStore table".
    pub subject: String,
    /// What needs to be done — the task body.
    pub description: String,
    pub status: TaskStatus,
    /// Task ids that must reach `Completed` before this one starts.
    /// Advisory metadata the model self-polices; the store does not enforce
    /// ordering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<TaskId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_as_str_round_trips_through_storage_str() {
        for status in TaskStatus::ALL {
            assert_eq!(TaskStatus::from_storage_str(status.as_str()), Some(status));
        }
    }

    #[test]
    fn status_as_str_matches_serde_snake_case() {
        let json = serde_json::to_string(&TaskStatus::InProgress).unwrap();
        assert_eq!(json, "\"in_progress\"");
        assert_eq!(json.trim_matches('"'), TaskStatus::InProgress.as_str());
    }

    #[test]
    fn unknown_storage_status_is_skipped() {
        assert_eq!(TaskStatus::from_storage_str("archived"), None);
    }

    #[test]
    fn task_serde_round_trips_and_omits_empty_depends_on() {
        let task = Task {
            id: TaskId::new(),
            subject: "Add the TaskStore table".into(),
            description: "Create the session_tasks table and wire the store".into(),
            status: TaskStatus::Pending,
            depends_on: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(
            !json.contains("depends_on"),
            "empty depends_on must be skipped"
        );
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(back, task);
    }

    #[test]
    fn task_deserializes_when_depends_on_absent() {
        let id = TaskId::new();
        let json = format!(
            r#"{{"id":"{id}","subject":"x","description":"y","status":"completed","created_at":"2026-06-04T00:00:00Z","updated_at":"2026-06-04T00:00:00Z"}}"#
        );
        let task: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert!(task.depends_on.is_empty());
        assert_eq!(task.subject, "x");
        assert_eq!(task.description, "y");
    }
}
