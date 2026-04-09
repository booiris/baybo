mod error;
mod operation;

pub use error::JobError;
pub use operation::OperationKind;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Result<T> = std::result::Result<T, JobError>;

/// Job lifecycle status with a fixed state machine.
///
/// ```text
/// Pending -> InProgress -> Completed -> Submitted -> Accepted
///                       \-> Failed
///                       \-> Stuck -> InProgress
///                                \-> Failed
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    InProgress,
    Completed,
    Submitted,
    Accepted,
    Failed,
    Stuck,
}

impl JobStatus {
    /// Returns the set of statuses reachable from `self`.
    pub fn allowed_transitions(&self) -> &'static [JobStatus] {
        match self {
            JobStatus::Pending => &[JobStatus::InProgress],
            JobStatus::InProgress => &[JobStatus::Completed, JobStatus::Failed, JobStatus::Stuck],
            JobStatus::Completed => &[JobStatus::Submitted],
            JobStatus::Submitted => &[JobStatus::Accepted],
            JobStatus::Stuck => &[JobStatus::InProgress, JobStatus::Failed],
            JobStatus::Accepted | JobStatus::Failed => &[],
        }
    }

    /// Check whether transitioning from `self` to `target` is legal.
    pub fn can_transition_to(&self, target: &JobStatus) -> bool {
        self.allowed_transitions().contains(target)
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            JobStatus::Pending => "Pending",
            JobStatus::InProgress => "InProgress",
            JobStatus::Completed => "Completed",
            JobStatus::Submitted => "Submitted",
            JobStatus::Accepted => "Accepted",
            JobStatus::Failed => "Failed",
            JobStatus::Stuck => "Stuck",
        };
        write!(f, "{s}")
    }
}

/// A tracked unit of asynchronous work within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub session_id: String,
    pub parent_job_id: Option<String>,
    pub kind: OperationKind,
    pub status: JobStatus,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub trace_span_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// A record of a single state transition for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTransition {
    pub job_id: String,
    pub from: JobStatus,
    pub to: JobStatus,
    pub reason: Option<String>,
    pub timestamp: DateTime<Utc>,
}
