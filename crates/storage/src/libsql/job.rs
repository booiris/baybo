use async_trait::async_trait;

use super::LibsqlPool;
use crate::job::JobStore;
use aura_job::{Job, JobError, JobStatus, JobTransition};

pub struct LibsqlJobStore {
    pool: LibsqlPool,
}

impl LibsqlJobStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl JobStore for LibsqlJobStore {
    async fn create(&self, job: &Job) -> aura_job::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(job)
            .map_err(|e| JobError::Storage(format!("failed to serialize job: {e}")))?;
        conn.execute(
            "INSERT INTO jobs (id, data) VALUES (?1, ?2)",
            libsql::params![job.id.clone(), data],
        )
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql insert error: {e}")))?;
        Ok(())
    }

    async fn get(&self, job_id: &str) -> aura_job::Result<Option<Job>> {
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
                let job: Job = serde_json::from_str(&data)
                    .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))?;
                Ok(Some(job))
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
                "UPDATE jobs SET data = ?1 WHERE id = ?2 AND deleted_at IS NULL",
                libsql::params![data, job.id.clone()],
            )
            .await
            .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql update error: {e}")))?;

        if rows_affected == 0 {
            return Err(JobError::NotFound(job.id.clone()));
        }
        Ok(())
    }

    async fn list_by_session(&self, session_id: &str) -> aura_job::Result<Vec<Job>> {
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
            let job: Job = serde_json::from_str(&data)
                .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))?;
            if job.session_id == session_id {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    async fn list_by_status(&self, status: JobStatus) -> aura_job::Result<Vec<Job>> {
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
            let job: Job = serde_json::from_str(&data)
                .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))?;
            if job.status == status {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    async fn list_children(&self, parent_job_id: &str) -> aura_job::Result<Vec<Job>> {
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
            let job: Job = serde_json::from_str(&data)
                .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))?;
            if job.parent_job_id.as_deref() == Some(parent_job_id) {
                jobs.push(job);
            }
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
            let job: Job = serde_json::from_str(&data)
                .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))?;
            jobs.push(job);
        }
        Ok(jobs)
    }

    async fn record_transition(&self, transition: &JobTransition) -> aura_job::Result<()> {
        let conn = self.pool.conn();
        let data = serde_json::to_string(transition)
            .map_err(|e| JobError::Storage(format!("failed to serialize transition: {e}")))?;
        conn.execute(
            "INSERT INTO job_transitions (job_id, data) VALUES (?1, ?2)",
            libsql::params![transition.job_id.clone(), data],
        )
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("libsql insert error: {e}")))?;
        Ok(())
    }

    async fn get_transitions(&self, job_id: &str) -> aura_job::Result<Vec<JobTransition>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM job_transitions WHERE job_id = ?1 AND deleted_at IS NULL",
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
            let transition: JobTransition = serde_json::from_str(&data)
                .map_err(|e| JobError::Storage(format!("failed to deserialize transition: {e}")))?;
            transitions.push(transition);
        }
        Ok(transitions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_job::OperationKind;

    fn test_job(id: &str) -> Job {
        let mut job = Job::new(
            "sess-1",
            OperationKind::LlmCall {
                model: "gpt-4".to_string(),
            },
            None,
        );
        job.id = id.to_string();
        job
    }

    #[tokio::test]
    async fn create_and_get() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        store.create(&test_job("j1")).await.unwrap();
        let job = store.get("j1").await.unwrap();
        assert!(job.is_some());
        assert_eq!(job.unwrap().id, "j1");
    }

    #[tokio::test]
    async fn save_updates_job() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let mut job = test_job("j2");
        store.create(&job).await.unwrap();

        job.start().unwrap();
        store.save(&job).await.unwrap();

        let loaded = store.get("j2").await.unwrap().unwrap();
        assert_eq!(loaded.status, JobStatus::InProgress);
        assert!(loaded.started_at.is_some());
    }

    #[tokio::test]
    async fn save_nonexistent_returns_not_found() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let job = test_job("nonexistent");
        let err = store.save(&job).await.unwrap_err();
        assert!(matches!(err, JobError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_by_session() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        store.create(&test_job("j3")).await.unwrap();
        store.create(&test_job("j4")).await.unwrap();
        let jobs = store.list_by_session("sess-1").await.unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[tokio::test]
    async fn record_and_get_transitions() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlJobStore::new(pool);
        let transition = JobTransition {
            job_id: "j5".to_string(),
            from: JobStatus::Pending,
            to: JobStatus::InProgress,
            reason: Some("started".to_string()),
            timestamp: chrono::Utc::now(),
        };
        store.record_transition(&transition).await.unwrap();
        let transitions = store.get_transitions("j5").await.unwrap();
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].to, JobStatus::InProgress);
    }
}
