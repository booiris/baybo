use async_trait::async_trait;

use super::LibsqlPool;
use crate::job::JobStore;
use aura_job::{Job, JobError, JobStatusKind, JobTransition};
use aura_model::{JobId, SessionId};

pub struct LibsqlJobStore {
    pool: LibsqlPool,
}

impl LibsqlJobStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

fn status_kind_str(k: JobStatusKind) -> &'static str {
    match k {
        JobStatusKind::Pending => "pending",
        JobStatusKind::InProgress => "in_progress",
        JobStatusKind::Stuck => "stuck",
        JobStatusKind::Cancelled => "cancelled",
        JobStatusKind::Failed => "failed",
        JobStatusKind::Completed => "completed",
    }
}

fn job_kind_str(k: aura_job::JobKind) -> &'static str {
    match k {
        aura_job::JobKind::UserChat => "user_chat",
        aura_job::JobKind::Cron => "cron",
        aura_job::JobKind::System => "system",
        aura_job::JobKind::Spawned => "spawned",
    }
}

fn deserialize_job(data: &str) -> aura_job::Result<Job> {
    serde_json::from_str(data)
        .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))
}

#[async_trait]
impl JobStore for LibsqlJobStore {
    async fn create(&self, job: &Job) -> aura_job::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(job)
            .map_err(|e| JobError::Storage(format!("failed to serialize job: {e}")))?;
        conn.execute(
            "INSERT INTO jobs \
             (id, session_id, parent_job_id, kind, status_kind, effective_soul_version, \
              created_at, started_at, ended_at, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            libsql::params![
                job.id.to_string(),
                job.session_id.as_str().to_string(),
                job.parent_job_id.map(|p| p.to_string()),
                job_kind_str(job.kind).to_string(),
                status_kind_str(job.status.kind()).to_string(),
                job.effective_soul_version.clone(),
                super::time::to_us(job.created_at),
                job.started_at.map(super::time::to_us),
                job.ended_at.map(super::time::to_us),
                data,
            ],
        )
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql insert error: {e}")))?;
        Ok(())
    }

    async fn get(&self, job_id: &JobId) -> aura_job::Result<Option<Job>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM jobs WHERE id = ?1 AND deleted_at IS NULL",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql row error: {e}")))?;

        match row {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                Ok(Some(deserialize_job(&data)?))
            }
            None => Ok(None),
        }
    }

    async fn save(&self, job: &Job) -> aura_job::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(job)
            .map_err(|e| JobError::Storage(format!("failed to serialize job: {e}")))?;

        let rows_affected = conn
            .execute(
                "UPDATE jobs SET status_kind = ?1, started_at = ?2, ended_at = ?3, \
                                  data = ?4 \
                 WHERE id = ?5 AND deleted_at IS NULL",
                libsql::params![
                    status_kind_str(job.status.kind()).to_string(),
                    job.started_at.map(super::time::to_us),
                    job.ended_at.map(super::time::to_us),
                    data,
                    job.id.to_string(),
                ],
            )
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql update error: {e}")))?;

        if rows_affected == 0 {
            return Err(JobError::NotFound(job.id.to_string()));
        }
        Ok(())
    }

    async fn list_by_session(&self, session_id: &SessionId) -> aura_job::Result<Vec<Job>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM jobs \
                 WHERE session_id = ?1 AND deleted_at IS NULL \
                 ORDER BY created_at",
                libsql::params![session_id.as_str().to_string()],
            )
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let mut jobs = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            jobs.push(deserialize_job(&data)?);
        }
        Ok(jobs)
    }

    async fn list_by_status_kind(&self, kind: JobStatusKind) -> aura_job::Result<Vec<Job>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM jobs \
                 WHERE status_kind = ?1 AND deleted_at IS NULL \
                 ORDER BY created_at",
                libsql::params![status_kind_str(kind).to_string()],
            )
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let mut jobs = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            jobs.push(deserialize_job(&data)?);
        }
        Ok(jobs)
    }

    async fn list_recoverable(&self) -> aura_job::Result<Vec<Job>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM jobs \
                 WHERE status_kind IN ('pending', 'in_progress', 'stuck') \
                   AND deleted_at IS NULL \
                 ORDER BY created_at",
                (),
            )
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let mut jobs = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            jobs.push(deserialize_job(&data)?);
        }
        Ok(jobs)
    }

    async fn list_children(&self, parent_job_id: &JobId) -> aura_job::Result<Vec<Job>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM jobs \
                 WHERE parent_job_id = ?1 AND deleted_at IS NULL \
                 ORDER BY created_at",
                libsql::params![parent_job_id.to_string()],
            )
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let mut jobs = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            jobs.push(deserialize_job(&data)?);
        }
        Ok(jobs)
    }

    async fn list_all(&self) -> aura_job::Result<Vec<Job>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query("SELECT data FROM jobs WHERE deleted_at IS NULL", ())
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let mut jobs = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            jobs.push(deserialize_job(&data)?);
        }
        Ok(jobs)
    }

    async fn record_transition(&self, transition: &JobTransition) -> aura_job::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(transition)
            .map_err(|e| JobError::Storage(format!("failed to serialize transition: {e}")))?;
        conn.execute(
            "INSERT INTO job_transitions (job_id, data) VALUES (?1, ?2)",
            libsql::params![transition.job_id.to_string(), data],
        )
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql insert error: {e}")))?;
        Ok(())
    }

    async fn get_transitions(&self, job_id: &JobId) -> aura_job::Result<Vec<JobTransition>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM job_transitions \
                 WHERE job_id = ?1 AND deleted_at IS NULL ORDER BY id",
                libsql::params![job_id.to_string()],
            )
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql query error: {e}")))?;

        let mut transitions = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql row error: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let t: JobTransition = serde_json::from_str(&data)
                .map_err(|e| JobError::Storage(format!("failed to deserialize transition: {e}")))?;
            transitions.push(t);
        }
        Ok(transitions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_job::{JobInput, JobStatus};
    use aura_model::ContentBlock;

    fn test_job() -> Job {
        Job::new(
            SessionId::from("sess-1"),
            JobInput::UserChat {
                content: vec![ContentBlock::Text("hi".into())],
            },
            "soul-v1",
            None,
        )
    }

    #[tokio::test]
    async fn create_and_get() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let j = test_job();
        store.create(&j).await.unwrap();
        let loaded = store.get(&j.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, j.id);
    }

    #[tokio::test]
    async fn save_updates_status() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let mut j = test_job();
        store.create(&j).await.unwrap();
        j.start().unwrap();
        store.save(&j).await.unwrap();
        let loaded = store.get(&j.id).await.unwrap().unwrap();
        assert!(matches!(loaded.status, JobStatus::InProgress));
        assert!(loaded.started_at.is_some());
    }

    #[tokio::test]
    async fn save_nonexistent_returns_not_found() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let j = test_job();
        let err = store.save(&j).await.unwrap_err();
        assert!(matches!(err, JobError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_by_session_filters() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let a = test_job();
        let b = test_job();
        store.create(&a).await.unwrap();
        store.create(&b).await.unwrap();
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
        let pending = test_job();
        store.create(&pending).await.unwrap();

        let mut in_progress = test_job();
        in_progress.start().unwrap();
        store.create(&in_progress).await.unwrap();

        let mut stuck = test_job();
        stuck.start().unwrap();
        stuck.stuck("hung").unwrap();
        store.create(&stuck).await.unwrap();

        let mut completed = test_job();
        completed.start().unwrap();
        completed
            .complete(aura_job::JobOutput::Message {
                content: vec![ContentBlock::Text("ok".into())],
            })
            .unwrap();
        store.create(&completed).await.unwrap();

        let recoverable = store.list_recoverable().await.unwrap();
        assert_eq!(recoverable.len(), 3);
        // Completed must be excluded
        for j in &recoverable {
            assert!(!matches!(j.status, JobStatus::Completed));
        }
    }

    #[tokio::test]
    async fn record_and_get_transitions() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let mut j = test_job();
        store.create(&j).await.unwrap();
        let t = j.start().unwrap();
        store.record_transition(&t).await.unwrap();
        let ts = store.get_transitions(&j.id).await.unwrap();
        assert_eq!(ts.len(), 1);
        assert!(matches!(ts[0].to, JobStatus::InProgress));
    }
}
