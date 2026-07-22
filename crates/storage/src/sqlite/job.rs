use async_trait::async_trait;
use rusqlite::OptionalExtension;
use rusqlite::types::Value;

use super::SqlitePool;
use baybo_model::{JobId, SessionId};
use baybo_store::job::Result;
use baybo_store::{JobRow, JobStore, SessionJobStats, StorageError};

pub struct SqliteJobStore {
    pool: SqlitePool,
}

impl SqliteJobStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const SELECT_COLS: &str = "id, session_id, parent_job_id, kind, status_kind, \
     created_at, started_at, ended_at, data";

/// The `jobs` columns exactly as sqlite hands them over, before any fallible
/// decoding (job-id parse, µs → `DateTime`). The row closure of `query_map`
/// can only surface `rusqlite` errors, so decoding happens after the collect.
type RawJobRow = (
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
    String,
);

fn raw_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawJobRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}

fn parse_job_id(s: String) -> anyhow::Result<JobId> {
    s.parse::<JobId>()
        .map_err(|e| anyhow::anyhow!("parse job id: {e}"))
}

fn job_row_from(raw: RawJobRow) -> anyhow::Result<JobRow> {
    let (id, session_id, parent_job_id, kind, status_kind, created_at, started_at, ended_at, data) =
        raw;
    Ok(JobRow {
        id: parse_job_id(id)?,
        session_id: SessionId::from(session_id),
        parent_job_id: parent_job_id.map(parse_job_id).transpose()?,
        kind,
        status_kind,
        created_at: super::time::from_us(created_at)
            .ok_or_else(|| anyhow::anyhow!("created_at: out of range"))?,
        started_at: started_at.and_then(super::time::from_us),
        ended_at: ended_at.and_then(super::time::from_us),
        data,
    })
}

#[async_trait]
impl JobStore for SqliteJobStore {
    async fn create(&self, job: &JobRow) -> Result<()> {
        let id = job.id.to_string();
        let session_id = job.session_id.as_str().to_string();
        let parent_job_id = job.parent_job_id.map(|p| p.to_string());
        let kind = job.kind.clone();
        let status_kind = job.status_kind.clone();
        let created_at = super::time::to_us(job.created_at);
        let started_at = job.started_at.map(super::time::to_us);
        let ended_at = job.ended_at.map(super::time::to_us);
        let data = job.data.clone();
        self.pool
            .interact("jobs.create", move |conn| {
                conn.execute(
                    "INSERT INTO jobs \
                     (id, session_id, parent_job_id, kind, status_kind, \
                      created_at, started_at, ended_at, data) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        id,
                        session_id,
                        parent_job_id,
                        kind,
                        status_kind,
                        created_at,
                        started_at,
                        ended_at,
                        data,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn get(&self, job_id: &JobId) -> Result<Option<JobRow>> {
        let id = job_id.to_string();
        self.pool
            .interact("jobs.get", move |conn| {
                let raw = conn
                    .query_row(
                        &format!("SELECT {SELECT_COLS} FROM jobs WHERE id = ?1"),
                        rusqlite::params![id],
                        raw_job_row,
                    )
                    .optional()?;
                raw.map(job_row_from).transpose()
            })
            .await
    }

    async fn save(&self, job: &JobRow) -> Result<()> {
        let status_kind = job.status_kind.clone();
        let started_at = job.started_at.map(super::time::to_us);
        let ended_at = job.ended_at.map(super::time::to_us);
        let data = job.data.clone();
        let id = job.id.to_string();
        let rows_affected = self
            .pool
            .interact("jobs.save", move |conn| {
                Ok(conn.execute(
                    "UPDATE jobs SET status_kind = ?1, started_at = ?2, ended_at = ?3, data = ?4 \
                     WHERE id = ?5",
                    rusqlite::params![status_kind, started_at, ended_at, data, id],
                )?)
            })
            .await?;
        if rows_affected == 0 {
            return Err(StorageError::NotFound(job.id.to_string()));
        }
        Ok(())
    }

    async fn list_by_session(&self, session_id: &SessionId) -> Result<Vec<JobRow>> {
        self.collect(
            "jobs.list_by_session",
            format!("SELECT {SELECT_COLS} FROM jobs WHERE session_id = ?1 ORDER BY created_at"),
            vec![Value::from(session_id.as_str().to_string())],
        )
        .await
    }

    async fn list_active_by_session(&self, session_id: &SessionId) -> Result<Vec<JobRow>> {
        self.collect(
            "jobs.list_active_by_session",
            format!(
                "SELECT {SELECT_COLS} FROM jobs \
                 WHERE session_id = ?1 AND status_kind IN ('pending', 'in_progress', 'stuck') \
                 ORDER BY created_at"
            ),
            vec![Value::from(session_id.as_str().to_string())],
        )
        .await
    }

    async fn list_by_status_kind(&self, status_kind: &str) -> Result<Vec<JobRow>> {
        self.collect(
            "jobs.list_by_status_kind",
            format!("SELECT {SELECT_COLS} FROM jobs WHERE status_kind = ?1 ORDER BY created_at"),
            vec![Value::from(status_kind.to_string())],
        )
        .await
    }

    async fn list_recoverable(&self) -> Result<Vec<JobRow>> {
        self.collect(
            "jobs.list_recoverable",
            format!(
                "SELECT {SELECT_COLS} FROM jobs \
                 WHERE status_kind IN ('pending', 'in_progress', 'stuck') ORDER BY created_at"
            ),
            vec![],
        )
        .await
    }

    async fn list_children(&self, parent_job_id: &JobId) -> Result<Vec<JobRow>> {
        self.collect(
            "jobs.list_children",
            format!("SELECT {SELECT_COLS} FROM jobs WHERE parent_job_id = ?1 ORDER BY created_at"),
            vec![Value::from(parent_job_id.to_string())],
        )
        .await
    }

    async fn list_all(&self) -> Result<Vec<JobRow>> {
        self.collect(
            "jobs.list_all",
            format!("SELECT {SELECT_COLS} FROM jobs"),
            vec![],
        )
        .await
    }

    async fn list_page(
        &self,
        status_kind: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<JobRow>, usize)> {
        let status_kind = status_kind.map(str::to_string);
        let limit = limit as i64;
        let offset = offset as i64;
        self.pool
            .interact("jobs.list_page", move |conn| {
                let (filter, params): (&str, Vec<Value>) = match &status_kind {
                    Some(k) => ("WHERE status_kind = ?1", vec![Value::from(k.clone())]),
                    None => ("", vec![]),
                };
                let total: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM jobs {filter}"),
                    rusqlite::params_from_iter(params.iter()),
                    |row| row.get(0),
                )?;
                let mut page_params = params;
                page_params.push(Value::from(limit));
                page_params.push(Value::from(offset));
                let (limit_ref, offset_ref) = if status_kind.is_some() {
                    ("?2", "?3")
                } else {
                    ("?1", "?2")
                };
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SELECT_COLS} FROM jobs {filter} \
                     ORDER BY created_at DESC, id DESC LIMIT {limit_ref} OFFSET {offset_ref}"
                ))?;
                let raws = stmt
                    .query_map(rusqlite::params_from_iter(page_params), raw_job_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let rows = raws
                    .into_iter()
                    .map(job_row_from)
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Ok((rows, total as usize))
            })
            .await
    }

    async fn count_by_status_kind(&self, status_kind: &str) -> Result<usize> {
        let status_kind = status_kind.to_string();
        self.pool
            .interact("jobs.count_by_status_kind", move |conn| {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM jobs WHERE status_kind = ?1",
                    rusqlite::params![status_kind],
                    |row| row.get(0),
                )?;
                Ok(n as usize)
            })
            .await
    }

    async fn session_job_stats(&self) -> Result<Vec<SessionJobStats>> {
        self.pool
            .interact("jobs.session_job_stats", move |conn| {
                // The correlated subquery resolves the newest job's
                // status_kind per group via idx_jobs_session
                // (session_id, created_at) — an index seek per session,
                // no data-blob reads.
                let mut stmt = conn.prepare(
                    "SELECT session_id, COUNT(*), \
                            (SELECT j2.status_kind FROM jobs j2 \
                              WHERE j2.session_id = jobs.session_id \
                              ORDER BY j2.created_at DESC, j2.id DESC LIMIT 1) \
                     FROM jobs GROUP BY session_id",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows
                    .into_iter()
                    .map(
                        |(session_id, job_count, latest_status_kind)| SessionJobStats {
                            session_id: SessionId::from(session_id),
                            job_count: job_count as usize,
                            latest_status_kind,
                        },
                    )
                    .collect())
            })
            .await
    }
}

impl SqliteJobStore {
    async fn collect(
        &self,
        op: &'static str,
        sql: String,
        params: Vec<Value>,
    ) -> Result<Vec<JobRow>> {
        self.pool
            .interact(op, move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let raws = stmt
                    .query_map(rusqlite::params_from_iter(params), raw_job_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                raws.into_iter().map(job_row_from).collect()
            })
            .await
    }
}

#[cfg(test)]
#[allow(unused_must_use)] // tests build state machines via direct calls; the JobTransition audit record isn't the assertion target
mod tests {
    use super::*;
    use baybo_job::{Job, JobInput, JobStatus};
    use baybo_model::{ContentBlock, TriggerKind};

    fn test_job() -> Job {
        Job::new(
            SessionId::from("sess-1"),
            TriggerKind::User,
            JobInput::UserChat {
                content: vec![ContentBlock::Text("hi".into())],
            },
            None,
        )
    }

    async fn create(store: &SqliteJobStore, job: &Job) {
        store.create(&job.to_row().unwrap()).await.unwrap();
    }

    async fn load(store: &SqliteJobStore, id: &JobId) -> Option<Job> {
        store
            .get(id)
            .await
            .unwrap()
            .map(|r| Job::from_row(r).unwrap())
    }

    #[tokio::test]
    async fn create_and_get() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteJobStore::new(pool);
        let j = test_job();
        create(&store, &j).await;
        assert_eq!(load(&store, &j.id).await.unwrap().id, j.id);
    }

    #[tokio::test]
    async fn save_updates_status() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteJobStore::new(pool);
        let mut j = test_job();
        create(&store, &j).await;
        j.start().unwrap();
        store.save(&j.to_row().unwrap()).await.unwrap();
        let loaded = load(&store, &j.id).await.unwrap();
        assert!(matches!(loaded.status, JobStatus::InProgress));
        assert!(loaded.started_at.is_some());
    }

    #[tokio::test]
    async fn save_nonexistent_returns_not_found() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteJobStore::new(pool);
        let j = test_job();
        let err = store.save(&j.to_row().unwrap()).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_by_session_filters() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteJobStore::new(pool);
        create(&store, &test_job()).await;
        create(&store, &test_job()).await;
        let jobs = store
            .list_by_session(&SessionId::from("sess-1"))
            .await
            .unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[tokio::test]
    async fn list_recoverable_includes_pending_in_progress_stuck() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteJobStore::new(pool);
        create(&store, &test_job()).await;

        let mut in_progress = test_job();
        in_progress.start().unwrap();
        create(&store, &in_progress).await;

        let mut stuck = test_job();
        stuck.start().unwrap();
        stuck.stuck("hung").unwrap();
        create(&store, &stuck).await;

        let mut completed = test_job();
        completed.start().unwrap();
        completed
            .complete(baybo_job::JobOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
                ordinal: None,
            })
            .unwrap();
        create(&store, &completed).await;

        let recoverable = store.list_recoverable().await.unwrap();
        assert_eq!(recoverable.len(), 3);
        for r in &recoverable {
            assert_ne!(r.status_kind, "completed");
        }
    }

    #[tokio::test]
    async fn list_active_by_session_scopes_and_filters_status() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteJobStore::new(pool);
        let sess_a = SessionId::from("sess-a");
        let sess_b = SessionId::from("sess-b");
        let mk = |s: &SessionId| {
            Job::new(
                s.clone(),
                TriggerKind::User,
                JobInput::UserChat {
                    content: vec![ContentBlock::Text("hi".into())],
                },
                None,
            )
        };

        // sess-a: pending + in_progress (both active) + completed (terminal).
        create(&store, &mk(&sess_a)).await;
        let mut a_running = mk(&sess_a);
        a_running.start().unwrap();
        create(&store, &a_running).await;
        let mut a_done = mk(&sess_a);
        a_done.start().unwrap();
        a_done
            .complete(baybo_job::JobOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
                ordinal: None,
            })
            .unwrap();
        create(&store, &a_done).await;
        // sess-b's in-flight job must NOT leak into sess-a's results.
        let mut b_running = mk(&sess_b);
        b_running.start().unwrap();
        create(&store, &b_running).await;

        let active = store.list_active_by_session(&sess_a).await.unwrap();
        assert_eq!(active.len(), 2, "only sess-a's non-terminal jobs");
        for j in &active {
            assert_eq!(j.session_id, sess_a);
            assert_ne!(j.status_kind, "completed");
        }
    }

    #[tokio::test]
    async fn session_job_stats_groups_and_picks_latest_status() {
        use std::collections::HashMap;

        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteJobStore::new(pool);
        let sess_a = SessionId::from("sess-a");
        let sess_b = SessionId::from("sess-b");
        let base = chrono::Utc::now();
        let mk = |s: &SessionId| {
            Job::new(
                s.clone(),
                TriggerKind::User,
                JobInput::UserChat {
                    content: vec![ContentBlock::Text("hi".into())],
                },
                None,
            )
        };

        // sess-a: completed job (older) + pending job (newest) — the
        // stats must report the newest job's status, not any other.
        let mut a_done = mk(&sess_a);
        a_done.created_at = base;
        a_done.start().unwrap();
        a_done
            .complete(baybo_job::JobOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
                ordinal: None,
            })
            .unwrap();
        create(&store, &a_done).await;
        let mut a_pending = mk(&sess_a);
        a_pending.created_at = base + chrono::Duration::seconds(1);
        create(&store, &a_pending).await;

        let mut b_running = mk(&sess_b);
        b_running.created_at = base;
        b_running.start().unwrap();
        create(&store, &b_running).await;

        let stats: HashMap<_, _> = store
            .session_job_stats()
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.session_id.clone(), s))
            .collect();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[&sess_a].job_count, 2);
        assert_eq!(stats[&sess_a].latest_status_kind, "pending");
        assert_eq!(stats[&sess_b].job_count, 1);
        assert_eq!(stats[&sess_b].latest_status_kind, "in_progress");
    }
}
