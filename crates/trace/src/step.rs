//! `Step` — one iteration of the agent loop, or one logical
//! work-unit (compression / memory / skill-selection / subagent).

use aura_model::{JobId, SessionId, StepId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::outcome::LifecycleOutcome;

/// One step in a job's life. Owns 1+ child `Span`s (in the spans table,
/// keyed by `step_id`). Steps within a job are strictly serial — there
/// is no `parallel_group` at the step level.
///
/// **Lifecycle invariant:** `outcome.is_terminal() ⟺ ended_at.is_some()`.
/// Mutate the two together via [`Step::close`]; never set one without
/// the other. Recovery, storage rewrites, and replay all rely on this
/// pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub job_id: JobId,
    pub kind: StepKind,
    pub started_at: DateTime<Utc>,
    /// Set once every child span (including parallel ones) has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: LifecycleOutcome,
}

impl Step {
    /// Atomically transition this step to a terminal outcome. Sets
    /// both `outcome` and `ended_at` from the single call so the
    /// lifecycle invariant cannot be violated. Returns an error if
    /// `outcome` is `LifecycleOutcome::Pending` (callers are expected
    /// to construct fresh steps with `Pending` directly and call
    /// `close` only at end time).
    pub fn close(&mut self, outcome: LifecycleOutcome, at: DateTime<Utc>) -> crate::Result<()> {
        if !outcome.is_terminal() {
            return Err(crate::TraceError::InvalidOperation(format!(
                "Step::close requires a terminal LifecycleOutcome, got {}",
                outcome.tag()
            )));
        }
        self.outcome = outcome;
        self.ended_at = Some(at);
        Ok(())
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
    /// Exactly one tool `Span`, no `LlmCall`. Used by direct-trigger
    /// paths (cron `TriggerAction::ToolCall`, system jobs that invoke
    /// a tool without LLM mediation). Splitting these out from
    /// `LlmIteration` keeps "LLM cost vs. direct-tool cost" cleanly
    /// separable in per-step aggregation.
    ToolDirect,
    Compression,
    MemoryRecall,
    MemoryWrite,
    SkillSelection,
    /// Special: the actual work runs in `child_session_id`. The step's
    /// inner span is a `SpanKind::SubagentStub` that records the parent's
    /// wait window only — no tool calls, no LLM calls live here.
    Subagent {
        child_session_id: SessionId,
        child_root_job_id: JobId,
    },
}

impl StepKind {
    pub fn tag(&self) -> &'static str {
        match self {
            StepKind::LlmIteration => "llm_iteration",
            StepKind::ToolDirect => "tool_direct",
            StepKind::Compression => "compression",
            StepKind::MemoryRecall => "memory_recall",
            StepKind::MemoryWrite => "memory_write",
            StepKind::SkillSelection => "skill_selection",
            StepKind::Subagent { .. } => "subagent",
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
            outcome: LifecycleOutcome::Pending,
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
    fn subagent_round_trips_with_child_refs() {
        let s = fresh_step(StepKind::Subagent {
            child_session_id: SessionId::from("cli-child"),
            child_root_job_id: JobId::new(),
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn close_pairs_outcome_and_ended_at() {
        let mut s = fresh_step(StepKind::LlmIteration);
        let now = Utc::now();
        s.close(LifecycleOutcome::Ok, now).unwrap();
        assert_eq!(s.outcome, LifecycleOutcome::Ok);
        assert_eq!(s.ended_at, Some(now));
    }

    #[test]
    fn close_rejects_pending() {
        let mut s = fresh_step(StepKind::Compression);
        let err = s.close(LifecycleOutcome::Pending, Utc::now()).unwrap_err();
        assert!(matches!(err, crate::TraceError::InvalidOperation(_)));
        assert!(s.ended_at.is_none());
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
