//! sqlite implementation of [`TaskStore`] — the per-session planning
//! checklist (`session_tasks`).

use async_trait::async_trait;
use baybo_model::{SessionId, Task, TaskId, TaskStatus};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::task::{Result, TaskPatch, TaskStore};

pub struct SqliteTaskStore {
    pool: SqlitePool,
}

impl SqliteTaskStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const SELECT_COLS: &str =
    "task_id, subject, description, status, depends_on, created_at, updated_at";

/// Parse one already-fetched row into a [`Task`]. `None` (with a `warn!`)
/// when the row's content can't be made into a valid task — an unknown
/// status from a future variant, a corrupt timestamp, or malformed
/// `depends_on` JSON. Skipping rather than erroring keeps `list` from 500ing
/// on a single bad row, mirroring the session-blob skip discipline.
fn try_task_from_row(row: &rusqlite::Row<'_>) -> Option<Task> {
    let task_id_s: String = row.get(0).ok()?;
    let subject: String = row.get(1).ok()?;
    let description: String = row.get(2).ok()?;
    let status_s: String = row.get(3).ok()?;
    let depends_on_s: String = row.get(4).ok()?;
    let created_at_us: i64 = row.get(5).ok()?;
    let updated_at_us: i64 = row.get(6).ok()?;

    let id = match task_id_s.parse::<TaskId>() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(task_id = %task_id_s, error = %e, "skipping session_tasks row: bad task_id");
            return None;
        }
    };
    let status = match TaskStatus::from_storage_str(&status_s) {
        Some(s) => s,
        None => {
            tracing::warn!(status = %status_s, %id, "skipping session_tasks row: unknown status");
            return None;
        }
    };
    let depends_on: Vec<TaskId> = match serde_json::from_str(&depends_on_s) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, %id, "skipping session_tasks row: bad depends_on json");
            return None;
        }
    };
    let created_at = super::time::from_us(created_at_us)?;
    let updated_at = super::time::from_us(updated_at_us)?;

    Some(Task {
        id,
        subject,
        description,
        status,
        depends_on,
        created_at,
        updated_at,
    })
}

fn depends_on_json(depends_on: &[TaskId]) -> Result<String> {
    serde_json::to_string(depends_on)
        .map_err(|e| StorageError::Storage(format!("serialize task depends_on: {e}")))
}

#[async_trait]
impl TaskStore for SqliteTaskStore {
    async fn create(&self, session_id: &SessionId, task: &Task) -> Result<()> {
        let session_id = session_id.as_str().to_string();
        let task_id = task.id.to_string();
        let subject = task.subject.clone();
        let description = task.description.clone();
        let status = task.status.as_str().to_string();
        let depends_on = depends_on_json(&task.depends_on)?;
        let created_at = super::time::to_us(task.created_at);
        let updated_at = super::time::to_us(task.updated_at);
        self.pool
            .interact("tasks.create", move |conn| {
                // Idempotent on (session_id, task_id): the caller mints the id, so a
                // storage-level retry re-sends identical values and lands as a no-op
                // overwrite rather than a duplicate-key failure.
                conn.execute(
                    "INSERT INTO session_tasks \
                         (session_id, task_id, subject, description, status, depends_on, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                     ON CONFLICT(session_id, task_id) DO UPDATE SET \
                         subject     = excluded.subject, \
                         description = excluded.description, \
                         status      = excluded.status, \
                         depends_on  = excluded.depends_on, \
                         updated_at  = excluded.updated_at",
                    rusqlite::params![
                        session_id,
                        task_id,
                        subject,
                        description,
                        status,
                        depends_on,
                        created_at,
                        updated_at,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn get(&self, session_id: &SessionId, task_id: &TaskId) -> Result<Option<Task>> {
        let session_id = session_id.as_str().to_string();
        let task_id = task_id.to_string();
        self.pool
            .interact("tasks.get", move |conn| {
                let sql = format!(
                    "SELECT {SELECT_COLS} FROM session_tasks WHERE session_id = ?1 AND task_id = ?2"
                );
                let row = conn
                    .query_row(&sql, rusqlite::params![session_id, task_id], |row| {
                        Ok(try_task_from_row(row))
                    })
                    .optional()?;
                Ok(row.flatten())
            })
            .await
    }

    async fn list(&self, session_id: &SessionId) -> Result<Vec<Task>> {
        let session_id = session_id.as_str().to_string();
        self.pool
            .interact("tasks.list", move |conn| {
                // created_at then task_id (ULID) for a stable order even when two
                // tasks share a microsecond timestamp.
                let sql = format!(
                    "SELECT {SELECT_COLS} FROM session_tasks WHERE session_id = ?1 \
                     ORDER BY created_at, task_id"
                );
                let mut stmt = conn.prepare(&sql)?;
                let out = stmt
                    .query_map(rusqlite::params![session_id], |row| {
                        Ok(try_task_from_row(row))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<Task>>();
                Ok(out)
            })
            .await
    }

    async fn update(&self, session_id: &SessionId, patch: &TaskPatch) -> Result<bool> {
        if patch.is_empty() {
            return Ok(self.get(session_id, &patch.task_id).await?.is_some());
        }
        // Build a sparse UPDATE touching only the patched columns + updated_at,
        // so a concurrent update to a *different* attribute of this task (or a
        // different task) doesn't stomp.
        let mut sets: Vec<String> = Vec::new();
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(status) = patch.status {
            params.push(rusqlite::types::Value::Text(status.as_str().to_string()));
            sets.push(format!("status = ?{}", params.len()));
        }
        if let Some(subject) = &patch.subject {
            params.push(rusqlite::types::Value::Text(subject.clone()));
            sets.push(format!("subject = ?{}", params.len()));
        }
        if let Some(description) = &patch.description {
            params.push(rusqlite::types::Value::Text(description.clone()));
            sets.push(format!("description = ?{}", params.len()));
        }
        if let Some(depends_on) = &patch.depends_on {
            params.push(rusqlite::types::Value::Text(depends_on_json(depends_on)?));
            sets.push(format!("depends_on = ?{}", params.len()));
        }
        params.push(rusqlite::types::Value::Integer(super::time::to_us(
            chrono::Utc::now(),
        )));
        sets.push(format!("updated_at = ?{}", params.len()));

        params.push(rusqlite::types::Value::Text(
            session_id.as_str().to_string(),
        ));
        let session_idx = params.len();
        params.push(rusqlite::types::Value::Text(patch.task_id.to_string()));
        let task_idx = params.len();
        let sql = format!(
            "UPDATE session_tasks SET {} WHERE session_id = ?{session_idx} AND task_id = ?{task_idx}",
            sets.join(", ")
        );
        let affected = self
            .pool
            .interact("tasks.update", move |conn| {
                Ok(conn.execute(&sql, rusqlite::params_from_iter(params))?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn delete(&self, session_id: &SessionId, task_id: &TaskId) -> Result<bool> {
        let session_id = session_id.as_str().to_string();
        let task_id = task_id.to_string();
        let affected = self
            .pool
            .interact("tasks.delete", move |conn| {
                Ok(conn.execute(
                    "DELETE FROM session_tasks WHERE session_id = ?1 AND task_id = ?2",
                    rusqlite::params![session_id, task_id],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::session::SqliteSessionStore;
    use baybo_model::{ChannelType, Session, SessionState, TaskStatus, TriggerSource, User};
    use baybo_store::SessionStore;
    use chrono::Utc;

    fn make_session(id: &str) -> Session {
        let now = Utc::now();
        Session {
            id: SessionId::from(id),
            user: User {
                id: format!("u-{id}"),
                name: None,
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            created_at: now,
            last_active: now,
            state: SessionState::default(),
            root_session_id: SessionId::from(id),
            trigger: TriggerSource::User,
            lineage: None,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        }
    }

    // µs-aligned so an exact round-trip survives the µs storage precision
    // (the schema-wide convention; sub-µs nanos are intentionally dropped).
    fn now_us() -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap()
    }

    fn make_task(subject: &str, status: TaskStatus) -> Task {
        let now = now_us();
        Task {
            id: TaskId::new(),
            subject: subject.into(),
            description: format!("{subject} — body"),
            status,
            depends_on: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    async fn store_with_session(id: &str) -> (SqliteTaskStore, SessionId) {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let sessions = SqliteSessionStore::new(pool.clone());
        sessions.save(&make_session(id)).await.unwrap();
        (SqliteTaskStore::new(pool), SessionId::from(id))
    }

    #[tokio::test]
    async fn create_then_get_round_trips() {
        let (store, sid) = store_with_session("s1").await;
        let mut task = make_task("write the table", TaskStatus::Pending);
        task.depends_on = vec![TaskId::new()];
        task.description = "create it with an index".into();
        store.create(&sid, &task).await.unwrap();

        let got = store.get(&sid, &task.id).await.unwrap().unwrap();
        assert_eq!(got, task);
        assert!(store.get(&sid, &TaskId::new()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_orders_by_created_at() {
        let (store, sid) = store_with_session("s2").await;
        let mut a = make_task("first", TaskStatus::Pending);
        let mut b = make_task("second", TaskStatus::Pending);
        a.created_at = "2026-06-01T00:00:00Z".parse().unwrap();
        b.created_at = "2026-06-02T00:00:00Z".parse().unwrap();
        // Insert out of order.
        store.create(&sid, &b).await.unwrap();
        store.create(&sid, &a).await.unwrap();
        let list = store.list(&sid).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].subject, "first");
        assert_eq!(list[1].subject, "second");
    }

    #[tokio::test]
    async fn update_is_sparse_and_row_isolated() {
        let (store, sid) = store_with_session("s3").await;
        let a = make_task("a", TaskStatus::Pending);
        let b = make_task("b", TaskStatus::Pending);
        store.create(&sid, &a).await.unwrap();
        store.create(&sid, &b).await.unwrap();

        // Patch only a's status; subject stays, b untouched.
        let mut patch = TaskPatch::new(a.id);
        patch.status = Some(TaskStatus::InProgress);
        assert!(store.update(&sid, &patch).await.unwrap());

        let got_a = store.get(&sid, &a.id).await.unwrap().unwrap();
        assert_eq!(got_a.status, TaskStatus::InProgress);
        assert_eq!(got_a.subject, "a");
        assert!(got_a.updated_at >= a.updated_at);
        let got_b = store.get(&sid, &b.id).await.unwrap().unwrap();
        assert_eq!(got_b.status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn update_absent_returns_false() {
        let (store, sid) = store_with_session("s4").await;
        let mut patch = TaskPatch::new(TaskId::new());
        patch.status = Some(TaskStatus::Completed);
        assert!(!store.update(&sid, &patch).await.unwrap());
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (store, sid) = store_with_session("s6").await;
        let t = make_task("x", TaskStatus::Pending);
        store.create(&sid, &t).await.unwrap();
        assert!(store.delete(&sid, &t.id).await.unwrap());
        assert!(!store.delete(&sid, &t.id).await.unwrap());
        assert!(store.get(&sid, &t.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tasks_cascade_delete_with_session() {
        let pool = SqlitePool::open_in_memory().await.unwrap();
        let sessions = SqliteSessionStore::new(pool.clone());
        let s = make_session("s-cascade");
        sessions.save(&s).await.unwrap();
        let store = SqliteTaskStore::new(pool);
        let t = make_task("doomed", TaskStatus::Pending);
        store.create(&s.id, &t).await.unwrap();
        assert!(store.get(&s.id, &t.id).await.unwrap().is_some());

        sessions.delete(&s.id).await.unwrap();
        assert!(
            store.list(&s.id).await.unwrap().is_empty(),
            "tasks must cascade-delete with the parent session"
        );
    }
}
