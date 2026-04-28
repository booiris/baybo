mod error;
mod operation;

pub use error::JobError;
pub use operation::{JobKind, OperationKind};

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, JobError>;

/// Job lifecycle status.
///
/// ```text
/// Pending -> InProgress -> Completed -> Submitted -> Accepted
///       |         |              \-> Failed
///       |         \-> Failed
///       |         \-> Cancelled        (user-initiated abort)
///       |         \-> Stuck -> InProgress
///       |                  \-> Failed
///       |                  \-> Abandoned (recovery gave up)
///       \-> Cancelled                   (cancel before start)
/// ```
///
/// Submitted/Accepted distinguish "agent finished" from "verifier
/// approved". `AcceptancePolicy::Auto` causes JobManager to walk both
/// transitions atomically on completion so user-facing chat does not
/// see the seam.
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
    /// User explicitly aborted this job.
    ///
    /// Distinct from `Failed` so cost/billing and failure-rate stats can
    /// exclude it; trace retains any partial spans for inspection.
    Cancelled,
    /// Recovery worker decided not to retry a `Stuck` job.
    Abandoned,
}

impl JobStatus {
    /// Returns the set of statuses reachable from `self`.
    pub fn allowed_transitions(&self) -> &'static [JobStatus] {
        match self {
            JobStatus::Pending => &[JobStatus::InProgress, JobStatus::Cancelled],
            JobStatus::InProgress => &[
                JobStatus::Completed,
                JobStatus::Failed,
                JobStatus::Stuck,
                JobStatus::Cancelled,
            ],
            JobStatus::Completed => &[JobStatus::Submitted],
            JobStatus::Submitted => &[JobStatus::Accepted, JobStatus::Failed],
            JobStatus::Stuck => &[
                JobStatus::InProgress,
                JobStatus::Failed,
                JobStatus::Cancelled,
                JobStatus::Abandoned,
            ],
            JobStatus::Accepted
            | JobStatus::Failed
            | JobStatus::Cancelled
            | JobStatus::Abandoned => &[],
        }
    }

    /// Check whether transitioning from `self` to `target` is legal.
    pub fn can_transition_to(&self, target: &JobStatus) -> bool {
        self.allowed_transitions().contains(target)
    }

    /// Whether this is a terminal status.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Accepted | JobStatus::Failed | JobStatus::Cancelled | JobStatus::Abandoned
        )
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
            JobStatus::Cancelled => "Cancelled",
            JobStatus::Abandoned => "Abandoned",
        };
        write!(f, "{s}")
    }
}

/// Who is responsible for moving a job from `Completed` → `Submitted`
/// or `Submitted` → `Accepted`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcceptancePolicy {
    /// Both transitions auto-fire as soon as the job reaches `Completed`.
    /// Default for chat turns, cron, and system actions.
    #[default]
    Auto,
    /// Submit auto-fires on completion; Accept waits for `acceptor`.
    AutoSubmit { acceptor: Acceptor },
    /// Both transitions wait for explicit triggers from the named parties.
    Manual {
        submitter: Acceptor,
        acceptor: Acceptor,
    },
}

/// A party that can transition a job in the acceptance flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Acceptor {
    /// Wait for an explicit user action (e.g. `/v1/jobs/{id}/accept`).
    User,
    /// Invoke a tool/skill that returns an accept/reject verdict.
    Validator { tool_id: String },
    /// Apply `default` after `after` elapses without an explicit signal.
    Timeout {
        #[serde(with = "duration_secs")]
        after: Duration,
        default: bool,
    },
}

/// What to do with a `Stuck` job after a system restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryPolicy {
    /// Recovery worker auto-resumes up to `max_attempts` times.
    AutoResume { max_attempts: u32 },
    /// Wait for an operator to manually resume.
    Manual,
    /// Move directly to `Abandoned`.
    Abandon,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        RecoveryPolicy::AutoResume { max_attempts: 3 }
    }
}

mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

/// A tracked unit of asynchronous work within a session.
///
/// `kind` is currently `OperationKind` (per-operation granularity:
/// one Job per LLM call, one per tool call, etc.). The plan tracks a
/// follow-up that switches `kind` to [`JobKind`] (turn-level: one
/// Job per user message / cron fire / system action / sub-agent
/// delegation) and removes the per-operation Jobs in favour of a
/// single turn-level Job referenced by every span via
/// `parent_job_id`.
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
    /// Acceptance flow policy. See `AcceptancePolicy`.
    #[serde(default)]
    pub acceptance: AcceptancePolicy,
    /// What to do if this job ends up `Stuck`.
    #[serde(default)]
    pub recovery: RecoveryPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<DateTime<Utc>>,
    /// Number of times this job has been recovered from `Stuck`.
    #[serde(default)]
    pub recovery_attempts: u32,
}

impl Job {
    /// Create a new job in `Pending` status with a generated UUID and
    /// default acceptance/recovery policies (`Auto` and `AutoResume`).
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
            acceptance: AcceptancePolicy::default(),
            recovery: RecoveryPolicy::default(),
            submitted_at: None,
            accepted_at: None,
            recovery_attempts: 0,
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

        if target == JobStatus::InProgress && self.started_at.is_none() {
            self.started_at = Some(now);
        }
        // Stamp the first time the job leaves the live phase. Submitted→Failed
        // (verifier rejection) keeps the original `completed_at` so audit shows
        // when the agent actually stopped working, not when the rejection landed.
        if matches!(
            target,
            JobStatus::Completed
                | JobStatus::Accepted
                | JobStatus::Failed
                | JobStatus::Cancelled
                | JobStatus::Abandoned
        ) && self.completed_at.is_none()
        {
            self.completed_at = Some(now);
        }
        if target == JobStatus::Submitted {
            self.submitted_at = Some(now);
        }
        if target == JobStatus::Accepted {
            self.accepted_at = Some(now);
        }
        if from == JobStatus::Stuck && target == JobStatus::InProgress {
            self.recovery_attempts = self.recovery_attempts.saturating_add(1);
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

    /// Transition to `Cancelled` with a reason. Legal from `Pending` or
    /// `InProgress`. Distinct from `fail()` so cost/billing can exclude
    /// cancelled jobs from failure-rate metrics.
    pub fn cancel(&mut self, reason: &str) -> Result<JobTransition> {
        self.transition(JobStatus::Cancelled, None, None, Some(reason.to_owned()))
    }

    /// Mark a `Stuck` job as `Abandoned` (recovery gave up).
    pub fn abandon(&mut self, reason: &str) -> Result<JobTransition> {
        self.transition(JobStatus::Abandoned, None, None, Some(reason.to_owned()))
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
    fn pending_transitions() {
        let s = JobStatus::Pending;
        assert!(s.can_transition_to(&JobStatus::InProgress));
        assert!(s.can_transition_to(&JobStatus::Cancelled));
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
        assert!(s.can_transition_to(&JobStatus::Cancelled));
        assert!(!s.can_transition_to(&JobStatus::Accepted));
        assert!(!s.can_transition_to(&JobStatus::Pending));
    }

    #[test]
    fn stuck_can_be_abandoned() {
        let s = JobStatus::Stuck;
        assert!(s.can_transition_to(&JobStatus::Abandoned));
        assert!(s.can_transition_to(&JobStatus::Cancelled));
    }

    #[test]
    fn submitted_to_failed_preserves_completed_at() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        job.complete(serde_json::json!(null)).unwrap();
        let original = job.completed_at;
        assert!(original.is_some());
        std::thread::sleep(std::time::Duration::from_millis(2));
        job.submit().unwrap();
        job.fail("verifier rejected").unwrap();
        assert_eq!(job.status, JobStatus::Failed);
        // completed_at must still point at the agent's actual completion,
        // not at the verifier rejection time.
        assert_eq!(job.completed_at, original);
    }

    #[test]
    fn cancelled_and_abandoned_are_terminal() {
        assert!(JobStatus::Cancelled.is_terminal());
        assert!(JobStatus::Abandoned.is_terminal());
        assert!(JobStatus::Cancelled.allowed_transitions().is_empty());
        assert!(JobStatus::Abandoned.allowed_transitions().is_empty());
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
        assert!(JobStatus::Cancelled.is_terminal());
        assert!(JobStatus::Abandoned.is_terminal());
        assert!(!JobStatus::Pending.is_terminal());
        assert!(!JobStatus::InProgress.is_terminal());
        assert!(!JobStatus::Completed.is_terminal());
        assert!(!JobStatus::Stuck.is_terminal());
    }

    #[test]
    fn cancel_from_pending() {
        let mut job = Job::new("s1", test_kind(), None);
        let t = job.cancel("user changed mind").unwrap();
        assert_eq!(t.from, JobStatus::Pending);
        assert_eq!(t.to, JobStatus::Cancelled);
        assert!(job.is_terminal());
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn cancel_from_in_progress() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        let t = job.cancel("user pressed escape").unwrap();
        assert_eq!(t.to, JobStatus::Cancelled);
        assert_eq!(t.reason.as_deref(), Some("user pressed escape"));
        assert!(job.is_terminal());
    }

    #[test]
    fn submit_and_accept_record_timestamps() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        job.complete(serde_json::json!(null)).unwrap();
        assert!(job.submitted_at.is_none());
        assert!(job.accepted_at.is_none());
        job.submit().unwrap();
        assert!(job.submitted_at.is_some());
        assert!(job.accepted_at.is_none());
        job.accept().unwrap();
        assert!(job.accepted_at.is_some());
    }

    #[test]
    fn recovery_attempts_increments_on_recover() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        assert_eq!(job.recovery_attempts, 0);
        job.stuck("hung").unwrap();
        job.recover("retrying").unwrap();
        assert_eq!(job.recovery_attempts, 1);
        job.stuck("hung again").unwrap();
        job.recover("second retry").unwrap();
        assert_eq!(job.recovery_attempts, 2);
    }

    #[test]
    fn abandon_from_stuck() {
        let mut job = Job::new("s1", test_kind(), None);
        job.start().unwrap();
        job.stuck("hung").unwrap();
        let t = job.abandon("max retries exceeded").unwrap();
        assert_eq!(t.to, JobStatus::Abandoned);
        assert!(job.is_terminal());
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn default_acceptance_is_auto() {
        let job = Job::new("s1", test_kind(), None);
        assert!(matches!(job.acceptance, AcceptancePolicy::Auto));
    }

    #[test]
    fn default_recovery_is_auto_resume_three() {
        let job = Job::new("s1", test_kind(), None);
        assert!(matches!(
            job.recovery,
            RecoveryPolicy::AutoResume { max_attempts: 3 }
        ));
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

    #[test]
    fn job_kind_serde_round_trip() {
        for kind in [
            JobKind::UserMessage,
            JobKind::CronExecution {
                cron_job_id: "cron-7".into(),
            },
            JobKind::SystemAction {
                trigger: "periodic_review".into(),
            },
            JobKind::SubAgentDelegation {
                tool_call_id: "call-99".into(),
            },
        ] {
            let s = serde_json::to_string(&kind).unwrap();
            let _: JobKind = serde_json::from_str(&s).unwrap();
        }
    }

    #[test]
    fn operation_kind_subagent_and_acceptance_round_trip() {
        let spawn = OperationKind::SubAgentSpawn {
            child_session_id: "child-sess".into(),
            child_job_id: "child-job".into(),
        };
        let s = serde_json::to_string(&spawn).unwrap();
        assert!(s.contains("sub_agent_spawn"));
        let _: OperationKind = serde_json::from_str(&s).unwrap();

        let accept = OperationKind::Acceptance {
            from: JobStatus::Submitted,
            to: JobStatus::Accepted,
        };
        let s = serde_json::to_string(&accept).unwrap();
        assert!(s.contains("acceptance"));
        let _: OperationKind = serde_json::from_str(&s).unwrap();
    }
}
