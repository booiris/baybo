//! Half-open span recovery utility.
//!
//! At system startup, the runtime asks the trace store for every span
//! that has `started_at` but no `ended_at` (and is not soft-deleted),
//! then rewrites each as `Cancelled { SystemCrash }`. This function
//! groups the result by `JobId` so `JobLifecycle::recover_interrupted`
//! can fold the IDs into the parent job's `partial_artifacts`.
//!
//! This module exposes the **types** used by recovery — the actual
//! storage scan + rewrite lives in `agent::trace::SpanRecorder`.

use std::collections::HashMap;

use aura_job::CancelReason;
use aura_model::{JobId, SpanId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::outcome::LifecycleOutcome;

/// One half-open span that the recovery scan rewrote.
#[must_use = "RecoveredSpan represents recovery work that must be folded into the parent job's partial_artifacts"]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredSpan {
    pub job_id: JobId,
    pub span_id: SpanId,
    /// What the scan decided to write. By contract this is always
    /// `LifecycleOutcome::Cancelled { reason: SystemCrash }` for now;
    /// kept as a field so future recovery policies (e.g. `Failed` for
    /// repeated-restart escalations) can extend without breaking the
    /// shape.
    pub new_outcome: LifecycleOutcome,
    /// When the recovery scan rewrote this span (the `ended_at` it
    /// stamped on the row).
    pub ended_at: DateTime<Utc>,
}

impl RecoveredSpan {
    /// Construct a recovery record for a half-open span found at
    /// startup. `now` is taken explicitly so test code can pin time.
    pub fn for_system_crash(job_id: JobId, span_id: SpanId, now: DateTime<Utc>) -> Self {
        Self {
            job_id,
            span_id,
            new_outcome: LifecycleOutcome::Cancelled {
                reason: CancelReason::SystemCrash,
            },
            ended_at: now,
        }
    }

    /// Convenience: `for_system_crash` with `Utc::now()`.
    pub fn for_system_crash_now(job_id: JobId, span_id: SpanId) -> Self {
        Self::for_system_crash(job_id, span_id, Utc::now())
    }
}

/// The result of one recovery scan, grouped by parent job for the
/// `JobLifecycle` to consume.
#[must_use = "RecoveryReport must be handed to JobLifecycle::recover_interrupted; dropping it skips the recovery audit"]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub recovered: Vec<RecoveredSpan>,
}

impl RecoveryReport {
    pub fn new(recovered: Vec<RecoveredSpan>) -> Self {
        Self { recovered }
    }

    /// Bucket the recovered spans by their parent job. `Vec` rather
    /// than `Vec<SpanId>` to preserve insertion order — useful when the
    /// scan returned spans in time order and we want to keep that for
    /// audit / display.
    pub fn group_by_job(&self) -> HashMap<JobId, Vec<SpanId>> {
        let mut by_job: HashMap<JobId, Vec<SpanId>> = HashMap::new();
        for r in &self.recovered {
            by_job.entry(r.job_id).or_default().push(r.span_id);
        }
        by_job
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_system_crash_records_cancel_reason() {
        let r = RecoveredSpan::for_system_crash_now(JobId::new(), SpanId::new());
        assert!(matches!(
            r.new_outcome,
            LifecycleOutcome::Cancelled {
                reason: CancelReason::SystemCrash,
            }
        ));
    }

    #[test]
    fn group_by_job_groups_correctly() {
        let job_a = JobId::new();
        let job_b = JobId::new();
        let span_a1 = SpanId::new();
        let span_a2 = SpanId::new();
        let span_b1 = SpanId::new();
        let now = Utc::now();
        let report = RecoveryReport::new(vec![
            RecoveredSpan::for_system_crash(job_a, span_a1, now),
            RecoveredSpan::for_system_crash(job_b, span_b1, now),
            RecoveredSpan::for_system_crash(job_a, span_a2, now),
        ]);
        let grouped = report.group_by_job();
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[&job_a], vec![span_a1, span_a2]);
        assert_eq!(grouped[&job_b], vec![span_b1]);
    }

    #[test]
    fn report_round_trips() {
        let r = RecoveryReport::new(vec![RecoveredSpan::for_system_crash_now(
            JobId::new(),
            SpanId::new(),
        )]);
        let s = serde_json::to_string(&r).unwrap();
        let back: RecoveryReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }
}
