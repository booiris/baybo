//! Conversions between the rich `Job` / `JobTransition` domain types and
//! the persistence rows (`baybo_store::JobRow` / `JobTransitionRow`).
//!
//! The `JobStore` trait itself lives in `baybo-store` and trades in rows,
//! so the job state machine stays in this crate while the store contract
//! sits alongside every other one. Callers convert at the boundary.

use crate::{Job, JobError, JobInputKind, JobTransition, Result};
use baybo_store::{JobRow, JobTransitionRow};

/// Snake-case string for the denormalised `jobs.kind` column. The column
/// is display-only (never filtered in SQL; `from_row` rebuilds the whole
/// `Job` from `data`), so it carries the input-kind projection.
fn job_input_kind_str(kind: JobInputKind) -> &'static str {
    match kind {
        JobInputKind::UserChat => "user_chat",
        JobInputKind::Cron => "cron",
        JobInputKind::System => "system",
        JobInputKind::Spawned => "spawned",
        JobInputKind::SubagentNotification => "subagent_notification",
    }
}

impl Job {
    /// Project this job into a persistence [`JobRow`]: the queryable
    /// columns plus the full job serialized into `data`.
    pub fn to_row(&self) -> Result<JobRow> {
        let data = serde_json::to_string(self)
            .map_err(|e| JobError::Storage(format!("failed to serialize job: {e}")))?;
        Ok(JobRow {
            id: self.id,
            session_id: self.session_id.clone(),
            parent_job_id: self.parent_job_id,
            kind: job_input_kind_str(self.input.input_kind()).to_string(),
            status_kind: self.status.kind().as_snake_case().to_string(),
            created_at: self.created_at,
            started_at: self.started_at,
            ended_at: self.ended_at,
            data,
        })
    }

    /// Reconstruct a job from its persistence row.
    pub fn from_row(row: JobRow) -> Result<Job> {
        serde_json::from_str(&row.data)
            .map_err(|e| JobError::Storage(format!("failed to deserialize job: {e}")))
    }
}

impl JobTransition {
    pub fn to_row(&self) -> Result<JobTransitionRow> {
        let data = serde_json::to_string(self)
            .map_err(|e| JobError::Storage(format!("failed to serialize transition: {e}")))?;
        Ok(JobTransitionRow {
            job_id: self.job_id,
            data,
        })
    }

    pub fn from_row(row: JobTransitionRow) -> Result<JobTransition> {
        serde_json::from_str(&row.data)
            .map_err(|e| JobError::Storage(format!("failed to deserialize transition: {e}")))
    }
}

impl From<baybo_store::StorageError> for JobError {
    fn from(e: baybo_store::StorageError) -> Self {
        match e {
            baybo_store::StorageError::NotFound(s) => JobError::NotFound(s),
            baybo_store::StorageError::Internal(e) => JobError::Internal(e),
            other => JobError::Storage(other.to_string()),
        }
    }
}
