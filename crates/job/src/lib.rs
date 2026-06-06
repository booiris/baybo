//! Job lifecycle types and orchestration — see `docs/modules/job.md`
//! for the design.
//!
//! Domain types (`Job`, `JobStatus`, `JobKind`, `CancelReason`,
//! `JobError`) and the `JobLifecycle` persistence orchestrator both
//! live here; the orchestrator wraps a `JobStore` with the cancel
//! state machine, terminal-event bus, and `JobId → CancellationToken`
//! registry that the in-flight execution path subscribes to.

mod cancel;
mod cancellation_registry;
mod error;
mod kind;
mod lifecycle;
mod store;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use aura_model::{JobId, SessionId, SpanId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use aura_store::{JobRow, JobStore, JobTransitionRow};
pub use cancel::CancelReason;
pub use cancellation_registry::{JobCancellationGuard, JobCancellationRegistry};
pub use error::JobError;
pub use kind::{JobInput, JobKind, JobOutput};
pub use lifecycle::{JobLifecycle, JobTerminalEvent};

pub type Result<T> = std::result::Result<T, JobError>;

// ── JobStatus ──────────────────────────────────────────────────────

/// Job lifecycle status.
///
/// ```text
/// Pending → InProgress → Completed
///                    \→ Stuck { reason } → InProgress
///                                       \→ Failed { reason }
///                                       \→ Cancelled { reason, partial_artifacts }
///                    \→ Failed { reason }
///                    \→ Cancelled { reason, partial_artifacts }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    InProgress,
    Stuck {
        reason: String,
    },
    Cancelled {
        reason: CancelReason,
        /// Spans that completed (or partially completed) before the
        /// cancel. Reserved for a future prompt-assembly preamble that
        /// surfaces them to the next job's LLM; no consumer reads this
        /// field today.
        partial_artifacts: Vec<SpanId>,
    },
    Failed {
        reason: String,
    },
    Completed,
}

/// Pure discriminator for `JobStatus`. Used to express the state
/// machine without having to construct concrete variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobStatusKind {
    Pending,
    InProgress,
    Stuck,
    Cancelled,
    Failed,
    Completed,
}

impl JobStatus {
    pub fn kind(&self) -> JobStatusKind {
        match self {
            JobStatus::Pending => JobStatusKind::Pending,
            JobStatus::InProgress => JobStatusKind::InProgress,
            JobStatus::Stuck { .. } => JobStatusKind::Stuck,
            JobStatus::Cancelled { .. } => JobStatusKind::Cancelled,
            JobStatus::Failed { .. } => JobStatusKind::Failed,
            JobStatus::Completed => JobStatusKind::Completed,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.kind().is_terminal()
    }

    pub fn needs_recovery(&self) -> bool {
        self.kind().needs_recovery()
    }
}

impl JobStatusKind {
    /// Set of statuses reachable from `self` via `Job::transition`.
    pub fn allowed_transitions(self) -> &'static [JobStatusKind] {
        use JobStatusKind::*;
        match self {
            Pending => &[InProgress, Cancelled, Failed],
            InProgress => &[Completed, Stuck, Failed, Cancelled],
            Stuck => &[InProgress, Failed, Cancelled],
            Completed | Failed | Cancelled => &[],
        }
    }

    pub fn can_transition_to(self, target: JobStatusKind) -> bool {
        self.allowed_transitions().contains(&target)
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatusKind::Completed | JobStatusKind::Failed | JobStatusKind::Cancelled
        )
    }

    pub fn needs_recovery(self) -> bool {
        matches!(
            self,
            JobStatusKind::Pending | JobStatusKind::InProgress | JobStatusKind::Stuck
        )
    }

    /// Snake-case wire tag, matching the serde `rename_all` on
    /// `JobStatus`. `Display` delegates here so formatted error
    /// messages and JSON wire payloads use the same identifier — no
    /// PascalCase-in-logs / snake_case-in-JSON mismatch.
    pub fn as_snake_case(self) -> &'static str {
        match self {
            JobStatusKind::Pending => "pending",
            JobStatusKind::InProgress => "in_progress",
            JobStatusKind::Stuck => "stuck",
            JobStatusKind::Cancelled => "cancelled",
            JobStatusKind::Failed => "failed",
            JobStatusKind::Completed => "completed",
        }
    }
}

impl std::fmt::Display for JobStatusKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_snake_case())
    }
}

// ── Job ─────────────────────────────────────────────────────────────

/// One externally-triggered unit of work. Lives within a `Session` and
/// owns a chain of `Step`s (in `aura-trace`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub session_id: SessionId,
    pub parent_job_id: Option<JobId>,

    pub kind: JobKind,
    pub input: JobInput,
    pub status: JobStatus,

    /// Final contractual output. Set when the job enters `Completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_result: Option<JobOutput>,

    /// Index of trace spans during this job that emitted user-visible
    /// messages. Content lives in the trace tree, not here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emitted_span_ids: Vec<SpanId>,

    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl Job {
    /// Construct a fresh job in `Pending` status.
    pub fn new(session_id: SessionId, input: JobInput, parent_job_id: Option<JobId>) -> Self {
        let kind = input.kind();
        Self {
            id: JobId::new(),
            session_id,
            parent_job_id,
            kind,
            input,
            status: JobStatus::Pending,
            final_result: None,
            emitted_span_ids: Vec::new(),
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// Apply a state transition. Validates against the state machine,
    /// mutates status / timestamps / final_result, and returns the
    /// audit record.
    pub fn transition(
        &mut self,
        target: JobStatus,
        final_result: Option<JobOutput>,
        reason: Option<String>,
    ) -> Result<JobTransition> {
        self.transition_at(target, final_result, reason, Utc::now())
    }

    /// Apply a state transition at an explicit point in time. Used by
    /// the boot-time recovery sweep to roll an orphaned `InProgress`
    /// job to `Cancelled { SystemCrash }` with `ended_at` set to the
    /// last observed activity (`max(child_step.ended_at)`) rather than
    /// the boot wall-clock — the process may have crashed hours or days
    /// before the next start, and using `Utc::now()` here would make
    /// duration metrics meaningless.
    ///
    /// Live callers should keep using [`Self::transition`]; only
    /// recovery code should reach for this variant.
    pub fn transition_at(
        &mut self,
        target: JobStatus,
        final_result: Option<JobOutput>,
        reason: Option<String>,
        at: DateTime<Utc>,
    ) -> Result<JobTransition> {
        let from = self.status.clone();
        if !from.kind().can_transition_to(target.kind()) {
            return Err(JobError::InvalidTransition(format!(
                "{} -> {} (job {})",
                from.kind(),
                target.kind(),
                self.id
            )));
        }

        if matches!(target, JobStatus::InProgress) && self.started_at.is_none() {
            self.started_at = Some(at);
        }
        if target.kind().is_terminal() {
            self.ended_at = Some(at);
        }
        // `final_result` is the contractual output of a successful run.
        // Reject Failed/Cancelled/Stuck targets that try to write one —
        // those carry their reason on the status variant itself; mixing
        // a `final_result` with a non-Completed terminal would corrupt
        // the audit invariant ("`final_result.is_some()` ⇔ `Completed`").
        if let Some(out) = final_result {
            if !matches!(target, JobStatus::Completed) {
                return Err(JobError::InvalidTransition(format!(
                    "{} -> {} carries a final_result but only Completed accepts one (job {})",
                    from.kind(),
                    target.kind(),
                    self.id
                )));
            }
            self.final_result = Some(out);
        }

        let to = target.clone();
        self.status = target;

        Ok(JobTransition {
            job_id: self.id,
            from,
            to,
            reason,
            timestamp: at,
        })
    }

    // -- Convenience transition methods --

    pub fn start(&mut self) -> Result<JobTransition> {
        self.transition(JobStatus::InProgress, None, None)
    }

    /// Move from `InProgress` to `Completed` with the final contractual
    /// output.
    pub fn complete(&mut self, output: JobOutput) -> Result<JobTransition> {
        self.transition(JobStatus::Completed, Some(output), None)
    }

    pub fn fail(&mut self, reason: impl Into<String>) -> Result<JobTransition> {
        let reason = reason.into();
        self.transition(
            JobStatus::Failed {
                reason: reason.clone(),
            },
            None,
            Some(reason),
        )
    }

    pub fn cancel(
        &mut self,
        reason: CancelReason,
        partial_artifacts: Vec<SpanId>,
    ) -> Result<JobTransition> {
        self.transition(
            JobStatus::Cancelled {
                reason,
                partial_artifacts,
            },
            None,
            None,
        )
    }

    /// Cancel at an explicit point in time. Used only by the boot-time
    /// recovery sweep — live cancels go through [`Self::cancel`].
    pub fn cancel_at(
        &mut self,
        reason: CancelReason,
        partial_artifacts: Vec<SpanId>,
        at: DateTime<Utc>,
    ) -> Result<JobTransition> {
        self.transition_at(
            JobStatus::Cancelled {
                reason,
                partial_artifacts,
            },
            None,
            None,
            at,
        )
    }

    pub fn stuck(&mut self, reason: impl Into<String>) -> Result<JobTransition> {
        let reason = reason.into();
        self.transition(
            JobStatus::Stuck {
                reason: reason.clone(),
            },
            None,
            Some(reason),
        )
    }

    /// `Stuck → InProgress` only. Reaching `InProgress` from `Pending` is
    /// the job of `start()`, which records the transition without a recovery
    /// reason; conflating the two would let `recover()` masquerade as a
    /// regular start and corrupt the recovery audit trail.
    pub fn recover(&mut self, reason: impl Into<String>) -> Result<JobTransition> {
        if !matches!(self.status, JobStatus::Stuck { .. }) {
            return Err(JobError::InvalidTransition(format!(
                "{} -> InProgress (job {}): recover() requires Stuck",
                self.status.kind(),
                self.id
            )));
        }
        self.transition(JobStatus::InProgress, None, Some(reason.into()))
    }
}

/// Audit record for a single state transition.
///
/// Dropping a `JobTransition` without persisting it loses the audit
/// trail the state machine exists to produce; the `JobLifecycle`
/// lifecycle methods (`start`/`complete`/`fail`/`cancel`/`stuck`/`recover`)
/// are the intended consumers.
#[must_use = "JobTransition is the audit trail; let JobLifecycle persist it via its lifecycle methods"]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTransition {
    pub job_id: JobId,
    pub from: JobStatus,
    pub to: JobStatus,
    pub reason: Option<String>,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
#[allow(unused_must_use)] // tests assert on `j.status` directly; the JobTransition audit record isn't relevant here
mod tests {
    use super::*;
    use aura_model::ContentBlock;

    fn user_chat_input() -> JobInput {
        JobInput::UserChat {
            content: vec![ContentBlock::Text("hi".into())],
        }
    }

    fn fresh_job() -> Job {
        Job::new(SessionId::from("cli-test"), user_chat_input(), None)
    }

    fn dummy_output() -> JobOutput {
        JobOutput::Message {
            content: vec![ContentBlock::Text("ok".into())],
        }
    }

    // -- JobStatusKind state machine --

    #[test]
    fn pending_can_start_or_be_cancelled_or_fail() {
        let s = JobStatusKind::Pending;
        assert!(s.can_transition_to(JobStatusKind::InProgress));
        assert!(s.can_transition_to(JobStatusKind::Cancelled));
        assert!(s.can_transition_to(JobStatusKind::Failed));
        assert!(!s.can_transition_to(JobStatusKind::Completed));
        assert!(!s.can_transition_to(JobStatusKind::Stuck));
    }

    #[test]
    fn in_progress_transitions() {
        let s = JobStatusKind::InProgress;
        assert!(s.can_transition_to(JobStatusKind::Completed));
        assert!(s.can_transition_to(JobStatusKind::Stuck));
        assert!(s.can_transition_to(JobStatusKind::Failed));
        assert!(s.can_transition_to(JobStatusKind::Cancelled));
        assert!(!s.can_transition_to(JobStatusKind::Pending));
    }

    #[test]
    fn terminal_kinds_have_no_transitions() {
        assert!(JobStatusKind::Completed.allowed_transitions().is_empty());
        assert!(JobStatusKind::Failed.allowed_transitions().is_empty());
        assert!(JobStatusKind::Cancelled.allowed_transitions().is_empty());
    }

    #[test]
    fn is_terminal_and_needs_recovery_are_complementary() {
        for k in [
            JobStatusKind::Pending,
            JobStatusKind::InProgress,
            JobStatusKind::Stuck,
            JobStatusKind::Cancelled,
            JobStatusKind::Failed,
            JobStatusKind::Completed,
        ] {
            assert_ne!(k.is_terminal(), k.needs_recovery());
        }
    }

    // -- Job::new --

    #[test]
    fn new_job_is_pending() {
        let j = fresh_job();
        assert!(matches!(j.status, JobStatus::Pending));
        assert_eq!(j.kind, JobKind::UserChat);
        assert!(j.started_at.is_none());
        assert!(j.ended_at.is_none());
        assert!(j.final_result.is_none());
        assert!(j.parent_job_id.is_none());
    }

    #[test]
    fn new_jobs_have_unique_ids() {
        let a = fresh_job();
        let b = fresh_job();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn job_kind_derived_from_input() {
        let j = Job::new(
            SessionId::from("s"),
            JobInput::Spawned {
                initial_prompt: vec![],
            },
            None,
        );
        assert_eq!(j.kind, JobKind::Spawned);
    }

    // -- Happy path --

    #[test]
    fn full_success_path() {
        let mut j = fresh_job();
        let t = j.start().unwrap();
        assert_eq!(t.from, JobStatus::Pending);
        assert!(matches!(t.to, JobStatus::InProgress));
        assert!(j.started_at.is_some());

        let t = j.complete(dummy_output()).unwrap();
        assert!(matches!(t.from, JobStatus::InProgress));
        assert!(matches!(j.status, JobStatus::Completed));
        assert!(j.is_terminal());
        assert!(j.ended_at.is_some());
        assert!(j.final_result.is_some());
    }

    #[test]
    fn fail_from_in_progress() {
        let mut j = fresh_job();
        j.start().unwrap();
        let t = j.fail("timeout").unwrap();
        assert!(matches!(t.to, JobStatus::Failed { .. }));
        assert!(j.is_terminal());
        assert!(j.ended_at.is_some());
    }

    #[test]
    fn cancel_from_in_progress_keeps_partial() {
        let mut j = fresh_job();
        j.start().unwrap();
        let span = SpanId::new();
        j.cancel(CancelReason::UserPreempt, vec![span]).unwrap();
        match &j.status {
            JobStatus::Cancelled {
                reason,
                partial_artifacts,
            } => {
                assert_eq!(*reason, CancelReason::UserPreempt);
                assert_eq!(partial_artifacts.as_slice(), &[span]);
            }
            _ => panic!("expected Cancelled"),
        }
        assert!(j.is_terminal());
    }

    #[test]
    fn cancel_from_pending() {
        let mut j = fresh_job();
        j.cancel(CancelReason::ParentDeleted, vec![]).unwrap();
        assert!(matches!(j.status, JobStatus::Cancelled { .. }));
    }

    #[test]
    fn stuck_then_recover() {
        let mut j = fresh_job();
        j.start().unwrap();
        j.stuck("hung").unwrap();
        assert!(matches!(j.status, JobStatus::Stuck { .. }));
        let t = j.recover("watchdog").unwrap();
        assert!(matches!(t.to, JobStatus::InProgress));
        assert_eq!(t.reason.as_deref(), Some("watchdog"));
    }

    #[test]
    fn stuck_then_cancel() {
        let mut j = fresh_job();
        j.start().unwrap();
        j.stuck("hung").unwrap();
        j.cancel(CancelReason::ParentCancelled, vec![]).unwrap();
        assert!(matches!(j.status, JobStatus::Cancelled { .. }));
    }

    #[test]
    fn cannot_complete_from_pending() {
        let mut j = fresh_job();
        let err = j.complete(dummy_output()).unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    #[test]
    fn recover_rejects_pending() {
        let mut j = fresh_job();
        let err = j.recover("oops").unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
        assert!(matches!(j.status, JobStatus::Pending));
    }

    #[test]
    fn recover_rejects_in_progress() {
        let mut j = fresh_job();
        j.start().unwrap();
        let err = j.recover("oops").unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
        assert!(matches!(j.status, JobStatus::InProgress));
    }

    #[test]
    fn cannot_transition_from_terminal() {
        let mut j = fresh_job();
        j.start().unwrap();
        j.fail("done").unwrap();
        let err = j.start().unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    // -- Serde --

    #[test]
    fn job_round_trips_through_serde() {
        let mut j = fresh_job();
        j.start().unwrap();
        j.complete(dummy_output()).unwrap();
        let s = serde_json::to_string(&j).unwrap();
        let back: Job = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, j.id);
        assert_eq!(back.kind, j.kind);
        assert_eq!(back.session_id, j.session_id);
    }

    #[test]
    fn job_status_round_trips_through_serde() {
        let s = JobStatus::Cancelled {
            reason: CancelReason::SystemCrash,
            partial_artifacts: vec![SpanId::new()],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: JobStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind(), s.kind());
    }
}
