mod error;
mod operation;

pub use error::JobError;
pub use operation::OperationKind;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

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

    /// Whether this is a terminal status (`Accepted` or `Failed`).
    pub fn is_terminal(&self) -> bool {
        matches!(self, JobStatus::Accepted | JobStatus::Failed)
    }

    /// Whether a job in this status needs recovery after a system restart.
    ///
    /// Returns `true` for any non-terminal status, since these jobs were
    /// interrupted before reaching a final state.
    pub fn needs_recovery(&self) -> bool {
        !self.is_terminal()
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

impl Job {
    /// Create a new job in `Pending` status with a generated UUID.
    pub fn new(session_id: &str, kind: OperationKind, parent_job_id: Option<&str>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_owned(),
            parent_job_id: parent_job_id.map(String::from),
            kind,
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

    /// Whether this job has reached a terminal state (`Accepted` or `Failed`).
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// Apply a state transition. Validates the transition against the state
    /// machine, mutates this job's status/timestamps/output/error, and returns
    /// the corresponding `JobTransition` record.
    pub fn transition(
        &mut self,
        target: JobStatus,
        output: Option<Value>,
        error: Option<String>,
        reason: Option<String>,
    ) -> Result<JobTransition> {
        if !self.status.can_transition_to(&target) {
            return Err(JobError::InvalidTransition(format!(
                "{} -> {} (job {})",
                self.status, target, self.id
            )));
        }

        let from = self.status.clone();
        let now = Utc::now();

        // Timestamp management
        if target == JobStatus::InProgress && self.started_at.is_none() {
            self.started_at = Some(now);
        }
        if matches!(
            target,
            JobStatus::Completed | JobStatus::Accepted | JobStatus::Failed
        ) {
            self.completed_at = Some(now);
        }

        self.status = target.clone();
        if let Some(o) = output {
            self.output = Some(o);
        }
        if let Some(e) = error {
            self.error = Some(e);
        }

        Ok(JobTransition {
            job_id: self.id.clone(),
            from,
            to: target,
            reason,
            timestamp: now,
        })
    }

    // -- Convenience transition methods --

    /// Transition from `Pending` to `InProgress`.
    pub fn start(&mut self) -> Result<JobTransition> {
        self.transition(JobStatus::InProgress, None, None, None)
    }

    /// Transition from `InProgress` to `Completed` with output.
    pub fn complete(&mut self, output: Value) -> Result<JobTransition> {
        self.transition(JobStatus::Completed, Some(output), None, None)
    }

    /// Transition from `Completed` to `Submitted`.
    pub fn submit(&mut self) -> Result<JobTransition> {
        self.transition(JobStatus::Submitted, None, None, None)
    }

    /// Transition from `Submitted` to `Accepted`.
    pub fn accept(&mut self) -> Result<JobTransition> {
        self.transition(JobStatus::Accepted, None, None, None)
    }

    /// Transition from `InProgress` or `Stuck` to `Failed` with error message.
    pub fn fail(&mut self, error: &str) -> Result<JobTransition> {
        self.transition(JobStatus::Failed, None, Some(error.to_owned()), None)
    }

    /// Transition from `InProgress` to `Stuck` with a reason.
    pub fn stuck(&mut self, reason: &str) -> Result<JobTransition> {
        self.transition(JobStatus::Stuck, None, None, Some(reason.to_owned()))
    }

    /// Recover from `Stuck` back to `InProgress` with a reason.
    pub fn recover(&mut self, reason: &str) -> Result<JobTransition> {
        self.transition(JobStatus::InProgress, None, None, Some(reason.to_owned()))
    }

    /// Mark this job as interrupted by a system restart.
    ///
    /// Only `InProgress` jobs are transitioned to `Stuck`. Jobs in other
    /// non-terminal states (`Pending`, `Completed`, `Submitted`, `Stuck`)
    /// are left unchanged — they can be retried or resumed without a
    /// state change.
    ///
    /// Returns `Some(transition)` if the status changed, `None` otherwise.
    pub fn mark_interrupted(&mut self) -> Result<Option<JobTransition>> {
        if self.status == JobStatus::InProgress {
            let t = self.transition(
                JobStatus::Stuck,
                None,
                None,
                Some("system restart: interrupted while in progress".to_owned()),
            )?;
            Ok(Some(t))
        } else {
            Ok(None)
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_kind() -> OperationKind {
        OperationKind::LlmCall {
            model: "test-model".into(),
        }
    }

    // -- JobStatus tests --

    #[test]
    fn pending_can_only_go_to_in_progress() {
        let s = JobStatus::Pending;
        assert!(s.can_transition_to(&JobStatus::InProgress));
        assert!(!s.can_transition_to(&JobStatus::Completed));
        assert!(!s.can_transition_to(&JobStatus::Failed));
        assert!(!s.can_transition_to(&JobStatus::Accepted));
    }

    #[test]
    fn in_progress_transitions() {
        let s = JobStatus::InProgress;
        assert!(s.can_transition_to(&JobStatus::Completed));
        assert!(s.can_transition_to(&JobStatus::Failed));
        assert!(s.can_transition_to(&JobStatus::Stuck));
        assert!(!s.can_transition_to(&JobStatus::Accepted));
        assert!(!s.can_transition_to(&JobStatus::Pending));
    }

    #[test]
    fn terminal_states_have_no_transitions() {
        assert!(JobStatus::Accepted.allowed_transitions().is_empty());
        assert!(JobStatus::Failed.allowed_transitions().is_empty());
    }

    #[test]
    fn is_terminal() {
        assert!(JobStatus::Accepted.is_terminal());
        assert!(JobStatus::Failed.is_terminal());
        assert!(!JobStatus::Pending.is_terminal());
        assert!(!JobStatus::InProgress.is_terminal());
        assert!(!JobStatus::Completed.is_terminal());
        assert!(!JobStatus::Stuck.is_terminal());
    }

    // -- Job constructor tests --

    #[test]
    fn new_job_is_pending() {
        let job = Job::new("s1", test_kind(), None);
        assert_eq!(job.status, JobStatus::Pending);
        assert!(!job.id.is_empty());
        assert_eq!(job.session_id, "s1");
        assert!(job.parent_job_id.is_none());
        assert!(job.started_at.is_none());
        assert!(job.completed_at.is_none());
    }

    #[test]
    fn new_job_with_parent() {
        let job = Job::new("s1", test_kind(), Some("parent-1"));
        assert_eq!(job.parent_job_id.as_deref(), Some("parent-1"));
    }

    #[test]
    fn new_jobs_have_unique_ids() {
        let a = Job::new("s1", test_kind(), None);
        let b = Job::new("s1", test_kind(), None);
        assert_ne!(a.id, b.id);
    }

    // -- State machine transition tests --

    #[test]
    fn full_success_path() {
        let mut job = Job::new("s1", test_kind(), None);

        let t1 = job.start().unwrap();
        assert_eq!(t1.from, JobStatus::Pending);
        assert_eq!(t1.to, JobStatus::InProgress);
        assert_eq!(job.status, JobStatus::InProgress);
        assert!(job.started_at.is_some());

        let t2 = job.complete(serde_json::json!({"ok": true})).unwrap();
        assert_eq!(t2.from, JobStatus::InProgress);
        assert_eq!(t2.to, JobStatus::Completed);
        assert!(job.output.is_some());
        assert!(job.completed_at.is_some());

        let t3 = job.submit().unwrap();
        assert_eq!(t3.from, JobStatus::Completed);
        assert_eq!(t3.to, JobStatus::Submitted);

        let t4 = job.accept().unwrap();
        assert_eq!(t4.from, JobStatus::Submitted);
        assert_eq!(t4.to, JobStatus::Accepted);
        assert!(job.is_terminal());
    }

    #[test]
    fn fail_from_in_progress() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();

        let t = job.fail("timeout").unwrap();
        assert_eq!(t.to, JobStatus::Failed);
        assert_eq!(job.error.as_deref(), Some("timeout"));
        assert!(job.is_terminal());
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn stuck_then_recover() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();

        let t1 = job.stuck("no response").unwrap();
        assert_eq!(t1.to, JobStatus::Stuck);
        assert_eq!(t1.reason.as_deref(), Some("no response"));

        let t2 = job.recover("retrying").unwrap();
        assert_eq!(t2.to, JobStatus::InProgress);
        assert_eq!(t2.reason.as_deref(), Some("retrying"));

        job.complete(serde_json::json!(null)).unwrap();
        job.submit().unwrap();
        job.accept().unwrap();
        assert!(job.is_terminal());
    }

    #[test]
    fn stuck_then_fail() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        job.stuck("hung").unwrap();

        let t = job.fail("unrecoverable").unwrap();
        assert_eq!(t.to, JobStatus::Failed);
        assert!(job.is_terminal());
    }

    #[test]
    fn cannot_complete_from_pending() {
        let mut job = Job::new("s1", test_kind(), None);
        let err = job.complete(serde_json::json!(null)).unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    #[test]
    fn cannot_accept_from_pending() {
        let mut job = Job::new("s1", test_kind(), None);
        let err = job.accept().unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    #[test]
    fn cannot_transition_from_terminal() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        job.fail("done").unwrap();

        let err = job.start().unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    #[test]
    fn started_at_set_only_on_first_start() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        let first_start = job.started_at;

        job.stuck("stalled").unwrap();
        job.recover("retry").unwrap();
        // started_at should not change on re-entry to InProgress
        assert_eq!(job.started_at, first_start);
    }

    #[test]
    fn transition_returns_correct_record() {
        let mut job = Job::new("s1", test_kind(), None);
        let t = job.start().unwrap();
        assert_eq!(t.job_id, job.id);
        assert_eq!(t.from, JobStatus::Pending);
        assert_eq!(t.to, JobStatus::InProgress);
        assert!(t.reason.is_none());
    }

    // -- needs_recovery tests --

    #[test]
    fn needs_recovery_for_non_terminal() {
        assert!(JobStatus::Pending.needs_recovery());
        assert!(JobStatus::InProgress.needs_recovery());
        assert!(JobStatus::Completed.needs_recovery());
        assert!(JobStatus::Submitted.needs_recovery());
        assert!(JobStatus::Stuck.needs_recovery());
    }

    #[test]
    fn no_recovery_for_terminal() {
        assert!(!JobStatus::Accepted.needs_recovery());
        assert!(!JobStatus::Failed.needs_recovery());
    }

    // -- mark_interrupted tests --

    #[test]
    fn mark_interrupted_in_progress_becomes_stuck() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();

        let t = job.mark_interrupted().unwrap();
        assert!(t.is_some());
        let t = t.unwrap();
        assert_eq!(t.from, JobStatus::InProgress);
        assert_eq!(t.to, JobStatus::Stuck);
        assert_eq!(
            t.reason.as_deref(),
            Some("system restart: interrupted while in progress")
        );
        assert_eq!(job.status, JobStatus::Stuck);
    }

    #[test]
    fn mark_interrupted_pending_unchanged() {
        let mut job = Job::new("s1", test_kind(), None);
        let t = job.mark_interrupted().unwrap();
        assert!(t.is_none());
        assert_eq!(job.status, JobStatus::Pending);
    }

    #[test]
    fn mark_interrupted_stuck_unchanged() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        job.stuck("hung").unwrap();

        let t = job.mark_interrupted().unwrap();
        assert!(t.is_none());
        assert_eq!(job.status, JobStatus::Stuck);
    }

    #[test]
    fn mark_interrupted_completed_unchanged() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        job.complete(serde_json::json!({"ok": true})).unwrap();

        let t = job.mark_interrupted().unwrap();
        assert!(t.is_none());
        assert_eq!(job.status, JobStatus::Completed);
    }

    #[test]
    fn mark_interrupted_terminal_unchanged() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        job.fail("done").unwrap();

        let t = job.mark_interrupted().unwrap();
        assert!(t.is_none());
        assert_eq!(job.status, JobStatus::Failed);
    }
}
