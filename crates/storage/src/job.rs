use async_trait::async_trait;

use aura_job::{Job, JobError, JobStatus, JobTransition};

pub type Result<T> = std::result::Result<T, JobError>;

/// Persistence layer for jobs and their transitions.
#[async_trait]
pub trait JobStore: Send + Sync {
    async fn create(&self, job: &Job) -> Result<()>;
    async fn get(&self, job_id: &str) -> Result<Option<Job>>;
    /// Persist the current state of a job (status, timestamps, output, error).
    async fn save(&self, job: &Job) -> Result<()>;
    async fn list_by_session(&self, session_id: &str) -> Result<Vec<Job>>;
    async fn list_by_status(&self, status: JobStatus) -> Result<Vec<Job>>;
    async fn list_children(&self, parent_job_id: &str) -> Result<Vec<Job>>;
    /// Return every stored job. Ordering is unspecified — callers sort as needed.
    async fn list_all(&self) -> Result<Vec<Job>>;
    async fn record_transition(&self, transition: &JobTransition) -> Result<()>;
    async fn get_transitions(&self, job_id: &str) -> Result<Vec<JobTransition>>;
}
