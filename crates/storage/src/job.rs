use async_trait::async_trait;
use aura_job::{Job, JobError, JobStatus, JobTransition};
use serde_json::Value;

pub type Result<T> = std::result::Result<T, JobError>;

/// Persistence layer for jobs and their transitions.
#[async_trait]
pub trait JobStore: Send + Sync {
    async fn create(&self, job: &Job) -> Result<()>;
    async fn get(&self, job_id: &str) -> Result<Option<Job>>;
    async fn update_status(
        &self,
        job_id: &str,
        status: JobStatus,
        output: Option<Value>,
        error: Option<String>,
    ) -> Result<()>;
    async fn list_by_session(&self, session_id: &str) -> Result<Vec<Job>>;
    async fn list_by_status(&self, status: JobStatus) -> Result<Vec<Job>>;
    async fn list_children(&self, parent_job_id: &str) -> Result<Vec<Job>>;
    async fn record_transition(&self, transition: &JobTransition) -> Result<()>;
    async fn get_transitions(&self, job_id: &str) -> Result<Vec<JobTransition>>;
}
