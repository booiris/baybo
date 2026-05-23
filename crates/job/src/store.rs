//! Conversions between the rich `Job` / `JobTransition` domain types and
//! the persistence rows (`aura_store::JobRow` / `JobTransitionRow`).
//!
//! The `JobStore` trait itself lives in `aura-store` and trades in rows,
//! so the job state machine stays in this crate while the store contract
//! sits alongside every other one. Callers convert at the boundary.

use crate::{Job, JobError, JobKind, JobTransition, Result};
use aura_store::{JobRow, JobTransitionRow};

fn job_kind_str(kind: JobKind) -> &'static str {
    match kind {
        JobKind::UserChat => "user_chat",
        JobKind::Cron => "cron",
        JobKind::System => "system",
        JobKind::Spawned => "spawned",
        JobKind::SubagentNotification => "subagent_notification",
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
            kind: job_kind_str(self.kind).to_string(),
            status_kind: self.status.kind().as_snake_case().to_string(),
            effective_soul_version: self.effective_soul_version.clone(),
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

impl From<aura_store::StorageError> for JobError {
    fn from(e: aura_store::StorageError) -> Self {
        match e {
            aura_store::StorageError::NotFound(s) => JobError::NotFound(s),
            aura_store::StorageError::Internal(e) => JobError::Internal(e),
            other => JobError::Storage(other.to_string()),
        }
    }
}
