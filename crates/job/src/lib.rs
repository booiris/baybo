//! Job lifecycle types — see `docs/modules/job.md` for the design.
//!
//! This crate is types-only: no async, no storage. The persistence /
//! hook orchestrator (`JobLifecycle`) lives in `agent::job`.

mod cancel;
mod drift;
mod error;
mod kind;
mod verification;

use aura_model::{JobId, SessionId, SpanId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use cancel::CancelReason;
pub use drift::DriftRecord;
pub use error::JobError;
pub use kind::{JobInput, JobKind, JobOutput};
pub use verification::{SubmissionChannel, VerificationOutcome, VerifierRef};

pub type Result<T> = std::result::Result<T, JobError>;

// ── JobStatus ──────────────────────────────────────────────────────

/// Job lifecycle status.
///
/// ```text
/// Pending → InProgress → Completed { verification }
///                    \→ Stuck { reason } → InProgress
///                                       \→ Failed { reason }
///                                       \→ Cancelled { reason, partial_artifacts }
///                    \→ Failed { reason }
///                    \→ Cancelled { reason, partial_artifacts }
/// ```
///
/// `Completed` is terminal w.r.t. actor execution. The inner
/// `verification` substate may continue to advance via
/// `Job::advance_verification` (`Submitted → Accepted | Rejected`)
/// for jobs declared with a `VerifierRef`. See `verification.rs`.
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
        /// cancel. The next job's prompt-assembly step renders these as
        /// a "previously completed steps:" preamble.
        partial_artifacts: Vec<SpanId>,
    },
    Failed {
        reason: String,
    },
    Completed {
        verification: VerificationOutcome,
    },
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
            JobStatus::Completed { .. } => JobStatusKind::Completed,
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
    /// Note: verification advancement (within `Completed`) goes through
    /// `Job::advance_verification` and is **not** modeled here.
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
    /// `JobStatus`. The `Display` impl uses PascalCase for human output.
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
        let s = match self {
            JobStatusKind::Pending => "Pending",
            JobStatusKind::InProgress => "InProgress",
            JobStatusKind::Stuck => "Stuck",
            JobStatusKind::Cancelled => "Cancelled",
            JobStatusKind::Failed => "Failed",
            JobStatusKind::Completed => "Completed",
        };
        f.write_str(s)
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

    /// Contractual output (the value `verification` judges against).
    /// Set when the job enters `Completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_result: Option<JobOutput>,

    /// Index of trace spans during this job that emitted user-visible
    /// messages. Content lives in the trace tree, not here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emitted_span_ids: Vec<SpanId>,

    /// Soul version actually loaded at this job's start time. Compare
    /// against `Session.bound_soul_version` to detect drift.
    pub effective_soul_version: String,

    /// Empty unless this job ran under a soul different from the
    /// session's `bound_soul_version`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance_drift: Vec<DriftRecord>,

    /// Declared at job creation. Required for any verification advance
    /// past `Unverified`. None means this job ends terminal at
    /// `Completed { Unverified }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier: Option<VerifierRef>,

    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl Job {
    /// Construct a fresh job in `Pending` status.
    pub fn new(
        session_id: SessionId,
        input: JobInput,
        effective_soul_version: impl Into<String>,
        parent_job_id: Option<JobId>,
        verifier: Option<VerifierRef>,
    ) -> Self {
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
            effective_soul_version: effective_soul_version.into(),
            provenance_drift: Vec::new(),
            verifier,
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
        let from = self.status.clone();
        if !from.kind().can_transition_to(target.kind()) {
            return Err(JobError::InvalidTransition(format!(
                "{} -> {} (job {})",
                from.kind(),
                target.kind(),
                self.id
            )));
        }

        let now = Utc::now();
        if matches!(target, JobStatus::InProgress) && self.started_at.is_none() {
            self.started_at = Some(now);
        }
        if target.kind().is_terminal() {
            self.ended_at = Some(now);
        }
        if let Some(out) = final_result {
            self.final_result = Some(out);
        }

        let to = target.clone();
        self.status = target;

        Ok(JobTransition {
            job_id: self.id,
            from,
            to,
            reason,
            timestamp: now,
        })
    }

    // -- Convenience transition methods --

    pub fn start(&mut self) -> Result<JobTransition> {
        self.transition(JobStatus::InProgress, None, None)
    }

    /// Move from `InProgress` to `Completed { Unverified }` with the
    /// final contractual output. The verification chain (if any)
    /// advances separately via `advance_verification`.
    pub fn complete(&mut self, output: JobOutput) -> Result<JobTransition> {
        self.transition(
            JobStatus::Completed {
                verification: VerificationOutcome::Unverified,
            },
            Some(output),
            None,
        )
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

    pub fn recover(&mut self, reason: impl Into<String>) -> Result<JobTransition> {
        self.transition(JobStatus::InProgress, None, Some(reason.into()))
    }

    /// Mark this job as interrupted by a system restart.
    ///
    /// `InProgress` jobs become `Stuck` (executing context lost). Other
    /// non-terminal statuses (`Pending`, `Stuck`, `Completed { Submitted }`)
    /// are left unchanged — they can resume without a state change. The
    /// trace recovery scan separately rewrites half-open spans as
    /// `Cancelled { SystemCrash }` and folds them into
    /// `partial_artifacts` of their parent job (handled by
    /// `JobLifecycle::recover_interrupted`).
    pub fn mark_interrupted(&mut self) -> Result<Option<JobTransition>> {
        if matches!(self.status, JobStatus::InProgress) {
            let t = self.transition(
                JobStatus::Stuck {
                    reason: "system restart: interrupted while in progress".to_owned(),
                },
                None,
                Some("system restart".to_owned()),
            )?;
            Ok(Some(t))
        } else {
            Ok(None)
        }
    }

    // -- Verification advancement --

    /// Move the verification substate forward inside `Completed`.
    /// Returns an audit record describing the advance.
    pub fn advance_verification(
        &mut self,
        target: VerificationOutcome,
    ) -> Result<VerificationTransition> {
        let JobStatus::Completed { verification } = &self.status else {
            return Err(JobError::InvalidVerificationAdvance(format!(
                "job {} not in Completed",
                self.id
            )));
        };
        if self.verifier.is_none() {
            return Err(JobError::VerificationNotAllowed(format!(
                "job {} has no declared verifier",
                self.id
            )));
        }
        if !verification.can_advance_to(&target) {
            return Err(JobError::InvalidVerificationAdvance(format!(
                "job {} cannot advance verification: {:?} -> {:?}",
                self.id, verification, target
            )));
        }
        let from = verification.clone();
        let to = target.clone();
        let now = Utc::now();
        self.status = JobStatus::Completed {
            verification: target,
        };
        Ok(VerificationTransition {
            job_id: self.id,
            from,
            to,
            timestamp: now,
        })
    }

    /// Convenience: move `Completed { Unverified }` → `Completed
    /// { Submitted }`. Requires `verifier` to be present.
    pub fn submit_verification(
        &mut self,
        channel: SubmissionChannel,
    ) -> Result<VerificationTransition> {
        // `Unverified` does not allow `can_advance_to`; we have a
        // separate kick-off for it that bypasses that check (since the
        // session-trigger semantics decide when verification *begins*,
        // not the verification machine itself).
        let JobStatus::Completed { verification } = &self.status else {
            return Err(JobError::InvalidVerificationAdvance(format!(
                "job {} not in Completed",
                self.id
            )));
        };
        if self.verifier.is_none() {
            return Err(JobError::VerificationNotAllowed(format!(
                "job {} has no declared verifier",
                self.id
            )));
        }
        if !matches!(verification, VerificationOutcome::Unverified) {
            return Err(JobError::InvalidVerificationAdvance(format!(
                "job {} verification already started: {:?}",
                self.id, verification
            )));
        }
        let from = verification.clone();
        let to = VerificationOutcome::Submitted {
            submitted_at: Utc::now(),
            channel,
        };
        let now = Utc::now();
        self.status = JobStatus::Completed {
            verification: to.clone(),
        };
        Ok(VerificationTransition {
            job_id: self.id,
            from,
            to,
            timestamp: now,
        })
    }
}

/// Audit record for a single state transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTransition {
    pub job_id: JobId,
    pub from: JobStatus,
    pub to: JobStatus,
    pub reason: Option<String>,
    pub timestamp: DateTime<Utc>,
}

/// Audit record for one step of the verification chain (separate from
/// `JobTransition` because it does not change `JobStatusKind`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationTransition {
    pub job_id: JobId,
    pub from: VerificationOutcome,
    pub to: VerificationOutcome,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::ContentBlock;

    fn user_chat_input() -> JobInput {
        JobInput::UserChat {
            content: vec![ContentBlock::Text("hi".into())],
        }
    }

    fn fresh_job() -> Job {
        Job::new(
            SessionId::from("cli-test"),
            user_chat_input(),
            "soul-v1",
            None,
            None,
        )
    }

    fn fresh_verifier_job() -> Job {
        Job::new(
            SessionId::from("cli-test"),
            user_chat_input(),
            "soul-v1",
            None,
            Some(VerifierRef::User {
                user_id: "u1".into(),
            }),
        )
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
        assert!(j.verifier.is_none());
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
            "soul-v1",
            None,
            None,
        );
        assert_eq!(j.kind, JobKind::Spawned);
    }

    // -- Happy path --

    #[test]
    fn full_success_path_no_verifier() {
        let mut j = fresh_job();
        let t = j.start().unwrap();
        assert_eq!(t.from, JobStatus::Pending);
        assert!(matches!(t.to, JobStatus::InProgress));
        assert!(j.started_at.is_some());

        let t = j.complete(dummy_output()).unwrap();
        assert!(matches!(t.from, JobStatus::InProgress));
        assert!(matches!(
            j.status,
            JobStatus::Completed {
                verification: VerificationOutcome::Unverified
            }
        ));
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
    fn cannot_transition_from_terminal() {
        let mut j = fresh_job();
        j.start().unwrap();
        j.fail("done").unwrap();
        let err = j.start().unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    // -- mark_interrupted --

    #[test]
    fn mark_interrupted_in_progress_becomes_stuck() {
        let mut j = fresh_job();
        j.start().unwrap();
        let t = j.mark_interrupted().unwrap().unwrap();
        assert!(matches!(t.from, JobStatus::InProgress));
        assert!(matches!(j.status, JobStatus::Stuck { .. }));
    }

    #[test]
    fn mark_interrupted_pending_unchanged() {
        let mut j = fresh_job();
        let t = j.mark_interrupted().unwrap();
        assert!(t.is_none());
        assert!(matches!(j.status, JobStatus::Pending));
    }

    #[test]
    fn mark_interrupted_terminal_unchanged() {
        let mut j = fresh_job();
        j.start().unwrap();
        j.fail("done").unwrap();
        let t = j.mark_interrupted().unwrap();
        assert!(t.is_none());
    }

    // -- Verification --

    #[test]
    fn cannot_submit_verification_without_verifier() {
        let mut j = fresh_job();
        j.start().unwrap();
        j.complete(dummy_output()).unwrap();
        let err = j.submit_verification(SubmissionChannel::Ui).unwrap_err();
        assert!(matches!(err, JobError::VerificationNotAllowed(_)));
    }

    #[test]
    fn submit_verification_requires_completed() {
        let mut j = fresh_verifier_job();
        let err = j.submit_verification(SubmissionChannel::Ui).unwrap_err();
        assert!(matches!(err, JobError::InvalidVerificationAdvance(_)));
    }

    #[test]
    fn full_verification_chain_accept() {
        let mut j = fresh_verifier_job();
        j.start().unwrap();
        j.complete(dummy_output()).unwrap();
        // From Unverified
        let t = j.submit_verification(SubmissionChannel::Ui).unwrap();
        assert!(matches!(t.from, VerificationOutcome::Unverified));
        assert!(matches!(t.to, VerificationOutcome::Submitted { .. }));
        // From Submitted to Accepted
        let t = j
            .advance_verification(VerificationOutcome::Accepted {
                decided_at: Utc::now(),
                by: VerifierRef::User {
                    user_id: "u1".into(),
                },
                evidence: serde_json::json!({}),
            })
            .unwrap();
        assert!(matches!(t.from, VerificationOutcome::Submitted { .. }));
        assert!(matches!(t.to, VerificationOutcome::Accepted { .. }));
    }

    #[test]
    fn full_verification_chain_reject() {
        let mut j = fresh_verifier_job();
        j.start().unwrap();
        j.complete(dummy_output()).unwrap();
        j.submit_verification(SubmissionChannel::Api).unwrap();
        let t = j
            .advance_verification(VerificationOutcome::Rejected {
                decided_at: Utc::now(),
                by: VerifierRef::User {
                    user_id: "u1".into(),
                },
                reason: "missing tests".into(),
            })
            .unwrap();
        assert!(matches!(t.to, VerificationOutcome::Rejected { .. }));
    }

    #[test]
    fn verification_cannot_skip_submitted() {
        let mut j = fresh_verifier_job();
        j.start().unwrap();
        j.complete(dummy_output()).unwrap();
        // Cannot jump from Unverified -> Accepted directly
        let err = j
            .advance_verification(VerificationOutcome::Accepted {
                decided_at: Utc::now(),
                by: VerifierRef::User {
                    user_id: "u1".into(),
                },
                evidence: serde_json::json!({}),
            })
            .unwrap_err();
        assert!(matches!(err, JobError::InvalidVerificationAdvance(_)));
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
