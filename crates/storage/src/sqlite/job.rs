use async_trait::async_trait;
use aura_job::JobError;
use aura_job::{Job, JobStatus, JobStore, JobTransition};
use serde_json::Value;

use super::SqlitePool;

pub struct SqliteJobStore {
    pool: SqlitePool,
}

impl SqliteJobStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl JobStore for SqliteJobStore {
    async fn create(&self, job: &Job) -> aura_job::Result<()> {
        let pool = self.pool.clone();
        let id = job.id.clone();
        let data = serde_json::to_string(job)
            .map_err(|e| JobError::Storage(format!("failed to serialize job: {e}")))?;
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            conn.execute(
                "INSERT INTO jobs (id, data) VALUES (?1, ?2)",
                rusqlite::params![id, data],
            )
            .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite insert error: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn get(&self, job_id: &str) -> aura_job::Result<Option<Job>> {
        let pool = self.pool.clone();
        let job_id = job_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM jobs WHERE id = ?1")
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let result: Option<String> = stmt
                .query_row(rusqlite::params![job_id], |row| row.get(0))
                .optional()
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            match result {
                Some(data) => {
                    let job: Job = serde_json::from_str(&data).map_err(|e| {
                        JobError::Storage(format!("failed to deserialize job: {e}"))
                    })?;
                    Ok(Some(job))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn update_status(
        &self,
        job_id: &str,
        status: JobStatus,
        output: Option<Value>,
        error: Option<String>,
    ) -> aura_job::Result<()> {
        let pool = self.pool.clone();
        let job_id = job_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM jobs WHERE id = ?1")
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let data: String = stmt
                .query_row(rusqlite::params![job_id], |row| row.get(0))
                .map_err(|e| JobError::Internal(anyhow::anyhow!("job not found: {e}")))?;
            let mut job: Job = serde_json::from_str(&data)
                .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))?;
            job.status = status;
            if let Some(out) = output {
                job.output = Some(out);
            }
            if let Some(err) = error {
                job.error = Some(err);
            }
            let updated = serde_json::to_string(&job)
                .map_err(|e| JobError::Storage(format!("failed to serialize job: {e}")))?;
            conn.execute(
                "UPDATE jobs SET data = ?1 WHERE id = ?2",
                rusqlite::params![updated, job_id],
            )
            .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite update error: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn list_by_session(&self, session_id: &str) -> aura_job::Result<Vec<Job>> {
        let pool = self.pool.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM jobs")
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let mut jobs = Vec::new();
            for row in rows {
                let data =
                    row.map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite row error: {e}")))?;
                let job: Job = serde_json::from_str(&data)
                    .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))?;
                if job.session_id == session_id {
                    jobs.push(job);
                }
            }
            Ok(jobs)
        })
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn list_by_status(&self, status: JobStatus) -> aura_job::Result<Vec<Job>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM jobs")
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let mut jobs = Vec::new();
            for row in rows {
                let data =
                    row.map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite row error: {e}")))?;
                let job: Job = serde_json::from_str(&data)
                    .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))?;
                if job.status == status {
                    jobs.push(job);
                }
            }
            Ok(jobs)
        })
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn list_children(&self, parent_job_id: &str) -> aura_job::Result<Vec<Job>> {
        let pool = self.pool.clone();
        let parent_id = parent_job_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM jobs")
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let mut jobs = Vec::new();
            for row in rows {
                let data =
                    row.map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite row error: {e}")))?;
                let job: Job = serde_json::from_str(&data)
                    .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))?;
                if job.parent_job_id.as_deref() == Some(&parent_id) {
                    jobs.push(job);
                }
            }
            Ok(jobs)
        })
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn record_transition(&self, transition: &JobTransition) -> aura_job::Result<()> {
        let pool = self.pool.clone();
        let job_id = transition.job_id.clone();
        let data = serde_json::to_string(transition)
            .map_err(|e| JobError::Storage(format!("failed to serialize transition: {e}")))?;
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            conn.execute(
                "INSERT INTO job_transitions (job_id, data) VALUES (?1, ?2)",
                rusqlite::params![job_id, data],
            )
            .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite insert error: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }

    async fn get_transitions(&self, job_id: &str) -> aura_job::Result<Vec<JobTransition>> {
        let pool = self.pool.clone();
        let job_id = job_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM job_transitions WHERE job_id = ?1")
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![job_id], |row| row.get::<_, String>(0))
                .map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite query error: {e}")))?;
            let mut transitions = Vec::new();
            for row in rows {
                let data =
                    row.map_err(|e| JobError::Internal(anyhow::anyhow!("sqlite row error: {e}")))?;
                let transition: JobTransition = serde_json::from_str(&data).map_err(|e| {
                    JobError::Storage(format!("failed to deserialize transition: {e}"))
                })?;
                transitions.push(transition);
            }
            Ok(transitions)
        })
        .await
        .map_err(|e| JobError::Internal(anyhow::anyhow!("spawn_blocking join error: {e}")))?
    }
}

trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_job::OperationKind;
    use chrono::Utc;

    fn test_job(id: &str) -> Job {
        Job {
            id: id.to_string(),
            session_id: "sess-1".to_string(),
            parent_job_id: None,
            kind: OperationKind::LlmCall {
                model: "gpt-4".to_string(),
            },
            status: JobStatus::Pending,
            input: None,
            output: None,
            error: None,
            trace_span_id: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
        }
    }

    #[tokio::test]
    async fn create_and_get() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteJobStore::new(pool);
        store.create(&test_job("j1")).await.unwrap();
        let job = store.get("j1").await.unwrap();
        assert!(job.is_some());
        assert_eq!(job.unwrap().id, "j1");
    }

    #[tokio::test]
    async fn update_status() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteJobStore::new(pool);
        store.create(&test_job("j2")).await.unwrap();
        store
            .update_status("j2", JobStatus::InProgress, None, None)
            .await
            .unwrap();
        let job = store.get("j2").await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::InProgress);
    }

    #[tokio::test]
    async fn list_by_session() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteJobStore::new(pool);
        store.create(&test_job("j3")).await.unwrap();
        store.create(&test_job("j4")).await.unwrap();
        let jobs = store.list_by_session("sess-1").await.unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[tokio::test]
    async fn record_and_get_transitions() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteJobStore::new(pool);
        let transition = JobTransition {
            job_id: "j5".to_string(),
            from: JobStatus::Pending,
            to: JobStatus::InProgress,
            reason: Some("started".to_string()),
            timestamp: Utc::now(),
        };
        store.record_transition(&transition).await.unwrap();
        let transitions = store.get_transitions("j5").await.unwrap();
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].to, JobStatus::InProgress);
    }
}
