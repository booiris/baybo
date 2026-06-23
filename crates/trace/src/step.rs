//! `Step` — one iteration of the agent loop, or one logical
//! work-unit (compression / memory / skill-selection / subagent).

use baybo_model::{JobId, StepId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::outcome::{LifecycleOutcome, LifecycleState};

/// One step in a job's life. Owns 1+ child `Span`s (in the spans table,
/// keyed by `step_id`). Steps under a job carry no `parallel_group`
/// (unlike spans) and are ordered purely by `started_at`. Their
/// wall-clock intervals are normally disjoint, but not guaranteed to be:
/// the detached progress-observer step is `tokio::spawn`ed at an iteration
/// boundary and runs concurrently with the next `LlmIteration`, so its
/// interval can overlap a sibling under the same job. Don't assume
/// non-overlapping step intervals — recovery closes each step
/// independently and the trace UI orders by `started_at` / keys by id.
///
/// **Lifecycle invariant:** `outcome.is_terminal() ⟺ ended_at.is_some()`.
/// Mutate the two together via [`Step::close`]; never set one without
/// the other. Recovery, storage rewrites, and replay all rely on this
/// pairing.
///
/// **Storage coupling:** the `steps` table in `baybo-storage` derives its
/// indexed `job_id` / `started_at` columns from this struct's serialized
/// JSON via `json_extract(data, '$.job_id')` (and `'$.started_at'`). Those
/// field names are load-bearing — renaming them or adding a container
/// `#[serde(rename_all)]` silently breaks `list_steps_by_job` and ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub job_id: JobId,
    pub kind: StepKind,
    pub started_at: DateTime<Utc>,
    /// Set once every child span (including parallel ones) has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: LifecycleState,
}

impl Step {
    /// Atomically transition this step to a terminal outcome. Sets
    /// both `outcome` and `ended_at` from the single call so the
    /// lifecycle invariant cannot be violated. The terminal-only
    /// [`LifecycleOutcome`] type makes "close with `Pending`"
    /// unrepresentable.
    pub fn close(&mut self, outcome: LifecycleOutcome, at: DateTime<Utc>) {
        self.outcome = LifecycleState::Done(outcome);
        self.ended_at = Some(at);
    }
}

/// Closed enum of every kind of step. Each variant carries the
/// step-level structural data; per-span data lives on the spans
/// themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    /// At least one `SpanKind::LlmCall` span (multiple on retry) plus
    /// any number of tool `Span`s the LLM dispatched. Tools may be
    /// parallel within the step (sharing a `ParallelGroup` on the spans).
    ///
    /// Invariant: a successful step has ≥ 1 `LlmCall` span. A failed
    /// step *may* have zero only when the trace-store write of the
    /// first `begin_span` itself errored.
    LlmIteration,
    Compression,
    MemoryRecall,
    MemoryWrite,
    SkillSelection,
    /// An out-of-band LLM call that summarizes the in-flight turn's
    /// progress for the user (the progress-observer Notice). Read-only:
    /// it reuses the turn's context but never writes back to it.
    ProgressObserver,
}

impl StepKind {
    pub fn tag(&self) -> &'static str {
        match self {
            StepKind::LlmIteration => "llm_iteration",
            StepKind::Compression => "compression",
            StepKind::MemoryRecall => "memory_recall",
            StepKind::MemoryWrite => "memory_write",
            StepKind::SkillSelection => "skill_selection",
            StepKind::ProgressObserver => "progress_observer",
        }
    }
}

/// Opaque handle returned by `SpanRecorder::begin_step`. Carries
/// enough context to call `end_step(handle, outcome)` later — including
/// the begin-time `kind` and `started_at` so the recorder can persist
/// the closed step without an extra SELECT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepHandle {
    pub step_id: StepId,
    pub job_id: JobId,
    pub kind: StepKind,
    pub started_at: DateTime<Utc>,
}

impl StepHandle {
    pub fn new(step_id: StepId, job_id: JobId, kind: StepKind, started_at: DateTime<Utc>) -> Self {
        Self {
            step_id,
            job_id,
            kind,
            started_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_step(kind: StepKind) -> Step {
        Step {
            id: StepId::new(),
            job_id: JobId::new(),
            kind,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleState::Pending,
        }
    }

    #[test]
    fn llm_iteration_round_trips() {
        let s = fresh_step(StepKind::LlmIteration);
        let json = serde_json::to_string(&s).unwrap();
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn compression_round_trips() {
        let s = fresh_step(StepKind::Compression);
        let json = serde_json::to_string(&s).unwrap();
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn close_pairs_outcome_and_ended_at() {
        let mut s = fresh_step(StepKind::LlmIteration);
        let now = Utc::now();
        s.close(LifecycleOutcome::Ok, now);
        assert_eq!(s.outcome, LifecycleState::Done(LifecycleOutcome::Ok));
        assert_eq!(s.ended_at, Some(now));
    }

    #[test]
    fn step_handle_is_constructible() {
        let h = StepHandle::new(
            StepId::new(),
            JobId::new(),
            StepKind::LlmIteration,
            Utc::now(),
        );
        assert_eq!(h, h.clone());
    }
}
