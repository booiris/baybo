use async_trait::async_trait;

use aura_job::{Job, JobError, JobStatusKind, JobTransition};
use aura_model::{JobId, SessionId};

pub type Result<T> = std::result::Result<T, JobError>;

/// Persistence layer for jobs and their transitions.
///
/// Status filtering uses `JobStatusKind` (the variant discriminator)
/// because `JobStatus` carries inner data — `Cancelled { reason, partial_artifacts }`,
/// `Stuck { reason }` — that is not part of the filter predicate.
#[async_trait]
pub trait JobStore: Send + Sync {
    async fn create(&self, job: &Job) -> Result<()>;
    async fn get(&self, job_id: &JobId) -> Result<Option<Job>>;
    /// Persist the current state of a job (status, timestamps,
    /// final_result, emitted_span_ids, provenance_drift).
    async fn save(&self, job: &Job) -> Result<()>;
    async fn list_by_session(&self, session_id: &SessionId) -> Result<Vec<Job>>;
    async fn list_by_status_kind(&self, kind: JobStatusKind) -> Result<Vec<Job>>;
    async fn list_children(&self, parent_job_id: &JobId) -> Result<Vec<Job>>;
    /// Return every stored job. Ordering unspecified — callers sort.
    async fn list_all(&self) -> Result<Vec<Job>>;
    /// Return jobs whose `JobStatusKind` is in the non-terminal set
    /// (`Pending` / `InProgress` / `Stuck`). Used by admin queries
    /// surfacing jobs that need attention.
    async fn list_recoverable(&self) -> Result<Vec<Job>>;

    /// Append a state-machine transition audit row.
    async fn record_transition(&self, transition: &JobTransition) -> Result<()>;
    async fn get_transitions(&self, job_id: &JobId) -> Result<Vec<JobTransition>>;
}
