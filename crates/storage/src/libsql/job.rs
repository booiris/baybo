use async_trait::async_trait;

use super::LibsqlPool;
use aura_model::{JobId, SessionId};
use aura_store::job::Result;
use aura_store::{JobRow, JobStore, JobTransitionRow, StorageError};

pub struct LibsqlJobStore {
    pool: LibsqlPool,
}

impl LibsqlJobStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

const SELECT_COLS: &str = "id, session_id, parent_job_id, kind, status_kind, \
     created_at, started_at, ended_at, data";

fn col_err(ctx: &str, e: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(anyhow::anyhow!("libsql {ctx}: {e}"))
}

fn parse_job_id(s: String) -> Result<JobId> {
    s.parse::<JobId>().map_err(|e| col_err("parse job id", e))
}

fn job_row_from(row: &libsql::Row) -> Result<JobRow> {
    let get_s = |i: i32| row.get::<String>(i).map_err(|e| col_err("get col", e));
    let get_os = |i: i32| {
        row.get::<Option<String>>(i)
            .map_err(|e| col_err("get col", e))
    };
    let get_oi = |i: i32| row.get::<Option<i64>>(i).map_err(|e| col_err("get col", e));
    Ok(JobRow {
        id: parse_job_id(get_s(0)?)?,
        session_id: SessionId::from(get_s(1)?),
        parent_job_id: get_os(2)?.map(parse_job_id).transpose()?,
        kind: get_s(3)?,
        status_kind: get_s(4)?,
        created_at: super::time::from_us(row.get::<i64>(5).map_err(|e| col_err("get col", e))?)
            .ok_or_else(|| col_err("created_at", "out of range"))?,
        started_at: get_oi(6)?.and_then(super::time::from_us),
        ended_at: get_oi(7)?.and_then(super::time::from_us),
        data: get_s(8)?,
    })
}

#[async_trait]
impl JobStore for LibsqlJobStore {
    async fn create(&self, job: &JobRow) -> Result<()> {
        self.pool
            .conn()
            .execute(
                "INSERT INTO jobs \
                 (id, session_id, parent_job_id, kind, status_kind, \
                  created_at, started_at, ended_at, data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                libsql::params![
                    job.id.to_string(),
                    job.session_id.as_str().to_string(),
                    job.parent_job_id.map(|p| p.to_string()),
                    job.kind.clone(),
                    job.status_kind.clone(),
                    super::time::to_us(job.created_at),
                    job.started_at.map(super::time::to_us),
                    job.ended_at.map(super::time::to_us),
                    job.data.clone(),
                ],
            )
            .await
            .map_err(|e| col_err("insert error", e))?;
        Ok(())
    }

    async fn get(&self, job_id: &JobId) -> Result<Option<JobRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                &format!("SELECT {SELECT_COLS} FROM jobs WHERE id = ?1"),
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| col_err("query error", e))?;
        match rows.next().await.map_err(|e| col_err("row error", e))? {
            Some(row) => Ok(Some(job_row_from(&row)?)),
            None => Ok(None),
        }
    }

    async fn save(&self, job: &JobRow) -> Result<()> {
        let rows_affected = self
            .pool
            .conn()
            .execute(
                "UPDATE jobs SET status_kind = ?1, started_at = ?2, ended_at = ?3, data = ?4 \
                 WHERE id = ?5",
                libsql::params![
                    job.status_kind.clone(),
                    job.started_at.map(super::time::to_us),
                    job.ended_at.map(super::time::to_us),
                    job.data.clone(),
                    job.id.to_string(),
                ],
            )
            .await
            .map_err(|e| col_err("update error", e))?;
        if rows_affected == 0 {
            return Err(StorageError::NotFound(job.id.to_string()));
        }
        Ok(())
    }

    async fn list_by_session(&self, session_id: &SessionId) -> Result<Vec<JobRow>> {
        self.collect(
            &format!("SELECT {SELECT_COLS} FROM jobs WHERE session_id = ?1 ORDER BY created_at"),
            libsql::params![session_id.as_str().to_string()],
        )
        .await
    }

    async fn list_active_by_session(&self, session_id: &SessionId) -> Result<Vec<JobRow>> {
        self.collect(
            &format!(
                "SELECT {SELECT_COLS} FROM jobs \
                 WHERE session_id = ?1 AND status_kind IN ('pending', 'in_progress', 'stuck') \
                 ORDER BY created_at"
            ),
            libsql::params![session_id.as_str().to_string()],
        )
        .await
    }

    async fn list_by_status_kind(&self, status_kind: &str) -> Result<Vec<JobRow>> {
        self.collect(
            &format!("SELECT {SELECT_COLS} FROM jobs WHERE status_kind = ?1 ORDER BY created_at"),
            libsql::params![status_kind.to_string()],
        )
        .await
    }

    async fn list_recoverable(&self) -> Result<Vec<JobRow>> {
        self.collect(
            &format!(
                "SELECT {SELECT_COLS} FROM jobs \
                 WHERE status_kind IN ('pending', 'in_progress', 'stuck') ORDER BY created_at"
            ),
            (),
        )
        .await
    }

    async fn list_children(&self, parent_job_id: &JobId) -> Result<Vec<JobRow>> {
        self.collect(
            &format!("SELECT {SELECT_COLS} FROM jobs WHERE parent_job_id = ?1 ORDER BY created_at"),
            libsql::params![parent_job_id.to_string()],
        )
        .await
    }

    async fn list_all(&self) -> Result<Vec<JobRow>> {
        self.collect(&format!("SELECT {SELECT_COLS} FROM jobs"), ())
            .await
    }

    async fn record_transition(&self, transition: &JobTransitionRow) -> Result<()> {
        self.pool
            .conn()
            .execute(
                "INSERT INTO job_transitions (job_id, data) VALUES (?1, ?2)",
                libsql::params![transition.job_id.to_string(), transition.data.clone()],
            )
            .await
            .map_err(|e| col_err("insert error", e))?;
        Ok(())
    }

    async fn get_transitions(&self, job_id: &JobId) -> Result<Vec<JobTransitionRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM job_transitions WHERE job_id = ?1 ORDER BY id",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| col_err("query error", e))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| col_err("row error", e))? {
            out.push(JobTransitionRow {
                job_id: *job_id,
                data: row.get::<String>(0).map_err(|e| col_err("get col", e))?,
            });
        }
        Ok(out)
    }
}

impl LibsqlJobStore {
    async fn collect(
        &self,
        sql: &str,
        params: impl libsql::params::IntoParams,
    ) -> Result<Vec<JobRow>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(sql, params)
            .await
            .map_err(|e| col_err("query error", e))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| col_err("row error", e))? {
            out.push(job_row_from(&row)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(unused_must_use)] // tests build state machines via direct calls; the JobTransition audit record isn't the assertion target
mod tests {
    use super::*;
    use aura_job::{Job, JobInput, JobStatus};
    use aura_model::ContentBlock;

    fn test_job() -> Job {
        Job::new(
            SessionId::from("sess-1"),
            JobInput::UserChat {
                content: vec![ContentBlock::Text("hi".into())],
            },
            None,
        )
    }

    async fn create(store: &LibsqlJobStore, job: &Job) {
        store.create(&job.to_row().unwrap()).await.unwrap();
    }

    async fn load(store: &LibsqlJobStore, id: &JobId) -> Option<Job> {
        store
            .get(id)
            .await
            .unwrap()
            .map(|r| Job::from_row(r).unwrap())
    }

    #[tokio::test]
    async fn create_and_get() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let j = test_job();
        create(&store, &j).await;
        assert_eq!(load(&store, &j.id).await.unwrap().id, j.id);
    }

    #[tokio::test]
    async fn save_updates_status() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let j = test_job();
        let err = store.save(&j.to_row().unwrap()).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_by_session_filters() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
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
            .complete(aura_job::JobOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let sess_a = SessionId::from("sess-a");
        let sess_b = SessionId::from("sess-b");
        let mk = |s: &SessionId| {
            Job::new(
                s.clone(),
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
            .complete(aura_job::JobOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
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
    async fn record_and_get_transitions() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let mut j = test_job();
        create(&store, &j).await;
        let t = j.start().unwrap();
        store.record_transition(&t.to_row().unwrap()).await.unwrap();
        let ts = store.get_transitions(&j.id).await.unwrap();
        assert_eq!(ts.len(), 1);
        let t0 = aura_job::JobTransition::from_row(ts.into_iter().next().unwrap()).unwrap();
        assert!(matches!(t0.to, JobStatus::InProgress));
    }
}
