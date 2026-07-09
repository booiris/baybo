//! Recovery helpers for orphaned trace rows and non-terminal jobs.
//!
//! When execution is dropped mid-flight (SIGTERM, SIGKILL, actor panic,
//! tokio task abort), the in-flight `with_step` / `with_job` futures may
//! disappear before their `end_step` / `lifecycle.cancel` calls run. The
//! tool span typically already wrote its row (it ran one await earlier),
//! but the surrounding step row, the LLM span row, and the job row can be
//! stuck in `Pending` / `InProgress` forever unless a recovery path closes
//! them.
//!
//! [`recover_orphaned_traces_and_jobs`] sweeps those orphans at next
//! boot, cascading bottom-up:
//!
//! 1. For each non-terminal job (`Pending` / `InProgress` / `Stuck`):
//!    1. For each step under it, walk its spans:
//!       - Pending span → close as `Cancelled { SystemCrash }` with
//!         `ended_at = max(span.events.last().at, span.started_at)`.
//!       - Terminal span → contribute its `ended_at` to the floor.
//!    2. If the step is pending, close it with
//!       `ended_at = max(child_spans.ended_at, step.started_at)`.
//!    3. Contribute the step's `ended_at` to the job-level floor.
//! 2. Cancel the job with
//!    `ended_at = max(child_steps.ended_at, job.started_at_or_created_at)`
//!    and `reason = SystemCrash`.
//! 3. Sweep unfinished trace rows under terminal jobs. Detached work
//!    (background compression, title generation, progress observers) can
//!    outlive the turn job that owns its step; if the process exits mid-pass,
//!    the job is already terminal, so recovery closes only the trace subtree
//!    and leaves the job status untouched.
//!
//! All timestamps come from observed activity — never `Utc::now()`.
//! The process may have crashed hours before the next boot; stamping
//! recovery time would distort every duration metric in the trace UI.
//!
//! [`recover_panicked_actor_session`] handles the same shape while the
//! process is still alive. In that case the actor task's `JoinError`
//! gives us a real crash instant, so pending spans, steps, and jobs are
//! closed at that crash time instead of pretending the last trace event
//! was the end of execution.
//!
//! The sweep is best-effort: per-job errors are logged at `warn` and
//! the loop continues. A `RecoverySummary` is returned so the caller
//! can log totals.

use std::collections::HashSet;
use std::sync::Arc;

use baybo_job::{CancelReason, JobLifecycle};
use baybo_model::{JobId, SessionId};
use baybo_trace::{LifecycleOutcome, LifecycleState, Span, Step, TraceError, TraceStore};
use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};

const RECOVERY_CANCEL_REASON: CancelReason = CancelReason::SystemCrash;

#[derive(Debug, Clone, Copy)]
enum RecoveryClock {
    /// Boot recovery cannot know when the prior process died, so it closes
    /// rows at the last observed child activity.
    ObservedActivity,
    /// In-process actor panic recovery knows when the task failed. Use that
    /// as the close time, clamped above each row's own start/child floor.
    CrashAt(DateTime<Utc>),
}

impl RecoveryClock {
    fn close_time(self, observed_floor: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            RecoveryClock::ObservedActivity => observed_floor,
            RecoveryClock::CrashAt(crash_at) => crash_at.max(observed_floor),
        }
    }
}

/// Counters returned by [`recover_orphaned_traces_and_jobs`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySummary {
    pub jobs_inspected: usize,
    pub jobs_cancelled: usize,
    pub steps_closed: usize,
    pub spans_closed: usize,
}

/// Sweep orphan trace rows and non-terminal jobs left behind by an
/// unclean shutdown. Best-effort: per-job errors are warn-logged; the
/// sweep continues with the next job.
///
/// Idempotent: a second call is a no-op once the first has reached every
/// non-terminal job and every unfinished detached trace row (steps and spans
/// are already terminal; the recoverable-job and unfinished-step listings come
/// back empty).
pub async fn recover_orphaned_traces_and_jobs(
    trace_store: Arc<dyn TraceStore>,
    job_lifecycle: Arc<JobLifecycle>,
) -> RecoverySummary {
    let mut summary = RecoverySummary::default();

    let jobs = match job_lifecycle.list_recoverable().await {
        Ok(js) => js,
        Err(e) => {
            warn!(error = %e, "recovery sweep: failed to list recoverable jobs");
            return summary;
        }
    };

    let mut recoverable_job_ids = HashSet::new();
    for job in jobs {
        recoverable_job_ids.insert(job.id);
        summary.jobs_inspected += 1;
        let job_started = job.started_at.unwrap_or(job.created_at);
        match close_job_subtree(
            &trace_store,
            &job.id,
            job_started,
            RecoveryClock::ObservedActivity,
        )
        .await
        {
            Ok((closed_steps, closed_spans, end_floor)) => {
                summary.steps_closed += closed_steps;
                summary.spans_closed += closed_spans;
                if !job.is_terminal() {
                    if let Err(e) = job_lifecycle
                        .cancel_at(&job.id, RECOVERY_CANCEL_REASON, Vec::new(), end_floor)
                        .await
                    {
                        warn!(
                            job_id = %job.id,
                            error = %e,
                            "recovery sweep: cancel_at failed; job left non-terminal"
                        );
                    } else {
                        summary.jobs_cancelled += 1;
                    }
                }
            }
            Err(e) => {
                warn!(
                    job_id = %job.id,
                    error = %e,
                    "recovery sweep: failed to close trace subtree; job left non-terminal"
                );
            }
        }
    }

    let detached_summary =
        recover_detached_trace_rows(&trace_store, &job_lifecycle, &recoverable_job_ids).await;
    summary.jobs_inspected += detached_summary.jobs_inspected;
    summary.steps_closed += detached_summary.steps_closed;
    summary.spans_closed += detached_summary.spans_closed;

    if summary.jobs_inspected > 0 {
        info!(
            jobs_inspected = summary.jobs_inspected,
            jobs_cancelled = summary.jobs_cancelled,
            steps_closed = summary.steps_closed,
            spans_closed = summary.spans_closed,
            "recovery sweep: closed orphan trace rows from prior process"
        );
    } else {
        debug!("recovery sweep: no orphan jobs or detached trace rows found");
    }

    summary
}

/// Recover active turn jobs for one actor that panicked while the
/// process stayed alive. This is the in-process counterpart to
/// [`recover_orphaned_traces_and_jobs`]: it only scans the panicked
/// session's active turn-kind jobs, closes their half-open trace rows at
/// `crash_at`, then cancels the jobs with `SystemCrash`.
///
/// `JobLifecycle::cancel_at` publishes the terminal lifecycle event, so
/// the TurnState projector remains the only producer of the web chat's
/// inactive edge.
pub async fn recover_panicked_actor_session(
    trace_store: Arc<dyn TraceStore>,
    job_lifecycle: Arc<JobLifecycle>,
    session_id: &SessionId,
    crash_at: DateTime<Utc>,
) -> RecoverySummary {
    let mut summary = RecoverySummary::default();

    let jobs = match job_lifecycle.list_active_turns_by_session(session_id).await {
        Ok(js) => js,
        Err(e) => {
            warn!(%session_id, error = %e, "actor crash recovery: failed to list active turns");
            return summary;
        }
    };

    if jobs.is_empty() {
        debug!(%session_id, "actor crash recovery: no active turn jobs found");
        return summary;
    }

    for job in jobs {
        summary.jobs_inspected += 1;
        let job_started = job.started_at.unwrap_or(job.created_at);
        match close_job_subtree(
            &trace_store,
            &job.id,
            job_started,
            RecoveryClock::CrashAt(crash_at),
        )
        .await
        {
            Ok((closed_steps, closed_spans, end_floor)) => {
                summary.steps_closed += closed_steps;
                summary.spans_closed += closed_spans;
                if let Err(e) = job_lifecycle
                    .cancel_at(&job.id, RECOVERY_CANCEL_REASON, Vec::new(), end_floor)
                    .await
                {
                    warn!(
                        %session_id,
                        job_id = %job.id,
                        error = %e,
                        "actor crash recovery: cancel_at failed; job left non-terminal"
                    );
                } else {
                    summary.jobs_cancelled += 1;
                }
            }
            Err(e) => {
                warn!(
                    %session_id,
                    job_id = %job.id,
                    error = %e,
                    "actor crash recovery: failed to close trace subtree; job left non-terminal"
                );
            }
        }
    }

    if summary.jobs_inspected > 0 {
        info!(
            %session_id,
            jobs_inspected = summary.jobs_inspected,
            jobs_cancelled = summary.jobs_cancelled,
            steps_closed = summary.steps_closed,
            spans_closed = summary.spans_closed,
            "actor crash recovery: closed orphan trace rows"
        );
    }

    summary
}

async fn recover_detached_trace_rows(
    trace_store: &Arc<dyn TraceStore>,
    job_lifecycle: &Arc<JobLifecycle>,
    recoverable_job_ids: &HashSet<JobId>,
) -> RecoverySummary {
    let mut summary = RecoverySummary::default();
    let rows = match trace_store.list_unfinished_steps().await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "recovery sweep: failed to list unfinished trace steps");
            return summary;
        }
    };

    let mut job_ids = HashSet::new();
    for row in rows {
        match Step::from_row(row) {
            Ok(step) if !recoverable_job_ids.contains(&step.job_id) => {
                job_ids.insert(step.job_id);
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "recovery sweep: failed to decode unfinished step row"),
        }
    }

    for job_id in job_ids {
        let job = match job_lifecycle.get(&job_id).await {
            Ok(Some(job)) => job,
            Ok(None) => {
                warn!(%job_id, "recovery sweep: unfinished trace row references missing job");
                continue;
            }
            Err(e) => {
                warn!(%job_id, error = %e, "recovery sweep: failed to load job for unfinished trace row");
                continue;
            }
        };

        if !job.is_terminal() {
            warn!(
                %job_id,
                status = %job.status.kind(),
                "recovery sweep: unfinished trace row belongs to non-terminal job missed by recoverable listing"
            );
            continue;
        }

        let job_started = job.started_at.unwrap_or(job.created_at);
        match close_job_subtree(
            trace_store,
            &job.id,
            job_started,
            RecoveryClock::ObservedActivity,
        )
        .await
        {
            Ok((closed_steps, closed_spans, _)) => {
                if closed_steps > 0 || closed_spans > 0 {
                    summary.jobs_inspected += 1;
                    summary.steps_closed += closed_steps;
                    summary.spans_closed += closed_spans;
                }
            }
            Err(e) => warn!(
                job_id = %job.id,
                error = %e,
                "recovery sweep: failed to close detached trace rows under terminal job"
            ),
        }
    }

    summary
}

/// Close every pending step / span under `job_id`, returning
/// `(steps_closed, spans_closed, end_floor)`. `end_floor` is
/// `max(child_steps.ended_at, job_started)` — the timestamp to stamp on
/// the job's eventual `Cancelled` transition.
async fn close_job_subtree(
    trace_store: &Arc<dyn TraceStore>,
    job_id: &JobId,
    job_started: DateTime<Utc>,
    clock: RecoveryClock,
) -> Result<(usize, usize, DateTime<Utc>), TraceError> {
    let mut steps_closed = 0usize;
    let mut spans_closed = 0usize;
    let mut job_end_floor = clock.close_time(job_started);

    let steps: Vec<Step> = trace_store
        .list_steps_by_job(job_id)
        .await?
        .into_iter()
        .map(Step::from_row)
        .collect::<std::result::Result<_, _>>()?;
    for step in steps {
        let step_started = step.started_at;
        let (closed_spans, step_end_floor) =
            close_step_spans(trace_store, &step, step_started, clock).await?;
        spans_closed += closed_spans;

        let step_ended_at = match (step.outcome.clone(), step.ended_at) {
            (LifecycleState::Done(_), Some(at)) => at,
            (LifecycleState::Done(_), None) => {
                // Pre-existing invariant violation — terminal outcome
                // without `ended_at`. Treat the span floor as the best
                // available signal and patch the row.
                let mut patched = step.clone();
                patched.ended_at = Some(step_end_floor);
                trace_store.save_step(&patched.to_row()?).await?;
                step_end_floor
            }
            (LifecycleState::Pending, _) => {
                let close_at = clock.close_time(step_end_floor);
                let mut closed = step.clone();
                closed.close(
                    LifecycleOutcome::Cancelled {
                        reason: RECOVERY_CANCEL_REASON,
                    },
                    close_at,
                );
                trace_store.save_step(&closed.to_row()?).await?;
                steps_closed += 1;
                close_at
            }
        };

        if step_ended_at > job_end_floor {
            job_end_floor = step_ended_at;
        }
    }

    Ok((steps_closed, spans_closed, job_end_floor))
}

/// Close every pending span under `step`, returning
/// `(spans_closed, step_end_floor)`. `step_end_floor` is
/// `max(child_spans.ended_at, step_started)`.
async fn close_step_spans(
    trace_store: &Arc<dyn TraceStore>,
    step: &Step,
    step_started: DateTime<Utc>,
    clock: RecoveryClock,
) -> Result<(usize, DateTime<Utc>), TraceError> {
    let spans: Vec<Span> = trace_store
        .list_spans_by_step(&step.id)
        .await?
        .into_iter()
        .map(Span::from_row)
        .collect::<std::result::Result<_, _>>()?;
    let mut spans_closed = 0usize;
    let mut step_end_floor = clock.close_time(step_started);

    for span in spans {
        let span_ended_at = match (span.outcome.clone(), span.ended_at) {
            (LifecycleState::Done(_), Some(at)) => at,
            (LifecycleState::Done(_), None) => {
                // Same invariant repair as for steps.
                let mut patched = span.clone();
                patched.ended_at = Some(span.started_at);
                trace_store.save_span(&patched.to_row()?).await?;
                span.started_at
            }
            (LifecycleState::Pending, _) => {
                let close_at = clock.close_time(pick_span_close_time(trace_store, &span).await?);
                let mut closed = span.clone();
                closed.close(
                    LifecycleOutcome::Cancelled {
                        reason: RECOVERY_CANCEL_REASON,
                    },
                    close_at,
                );
                trace_store.save_span(&closed.to_row()?).await?;
                spans_closed += 1;
                close_at
            }
        };

        if span_ended_at > step_end_floor {
            step_end_floor = span_ended_at;
        }
    }

    Ok((spans_closed, step_end_floor))
}

/// Pick the close time for an orphan pending span:
/// `max(span.events.last().at, span.started_at)`. SpanEvents are
/// ordered by `seq` at write time but we don't rely on that here —
/// take the explicit max.
async fn pick_span_close_time(
    trace_store: &Arc<dyn TraceStore>,
    span: &Span,
) -> Result<DateTime<Utc>, TraceError> {
    let events: Vec<baybo_trace::SpanEvent> = trace_store
        .list_span_events(&span.id)
        .await?
        .into_iter()
        .map(baybo_trace::SpanEvent::from_row)
        .collect::<std::result::Result<_, _>>()?;
    let mut at = span.started_at;
    for ev in &events {
        if ev.at > at {
            at = ev.at;
        }
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_job::test_support::MemoryJobStore;
    use baybo_job::{Job, JobInput, JobStatus, JobStore};
    use baybo_model::{
        ApprovalDecision, ContentBlock, ParallelGroup, ResourceAccess, SessionId, SpanId, StepId,
        TriggerKind,
    };
    use baybo_trace::test_support::MemoryTraceStore;
    use baybo_trace::{
        LlmCallBegin, LlmCallInputs, SpanEvent, SpanEventKind, SpanKind, StepKind, ToolCallBegin,
    };
    use chrono::Duration;

    fn pending_step(job_id: JobId, started_at: DateTime<Utc>) -> Step {
        Step {
            id: StepId::new(),
            job_id,
            kind: StepKind::LlmIteration,
            started_at,
            ended_at: None,
            outcome: LifecycleState::Pending,
        }
    }

    fn llm_span_kind() -> SpanKind {
        SpanKind::LlmCall {
            begin: LlmCallBegin {
                model_id: "test-model".into(),
                provider: "test".into(),
                provider_config_hash: String::new(),
                input_messages: LlmCallInputs::empty(),
                temperature: None,
            },
            result: None,
        }
    }

    fn tool_span_kind() -> SpanKind {
        SpanKind::ToolCall {
            begin: ToolCallBegin {
                tool_name: "test_tool".into(),
                tool_artifact_hash: String::new(),
                triggered_by: None,
                params: serde_json::Value::Null,
            },
            result: None,
        }
    }

    fn make_span(
        step_id: StepId,
        kind: SpanKind,
        started_at: DateTime<Utc>,
        outcome: LifecycleState,
        ended_at: Option<DateTime<Utc>>,
    ) -> Span {
        Span {
            id: SpanId::new(),
            step_id,
            kind,
            parallel_group: None,
            started_at,
            ended_at,
            outcome,
            events: Vec::new(),
        }
    }

    async fn build_lifecycle_with_in_progress_job(
        started: DateTime<Utc>,
    ) -> (Arc<JobLifecycle>, Job) {
        let store = Arc::new(MemoryJobStore::new());
        let mut job = Job::new(
            SessionId::from("s1"),
            TriggerKind::User,
            JobInput::UserChat {
                content: vec![ContentBlock::Text("hi".into())],
            },
            None,
        );
        job.status = JobStatus::InProgress;
        job.started_at = Some(started);
        store.create(&job.to_row().unwrap()).await.unwrap();
        let lifecycle = Arc::new(JobLifecycle::new(store));
        (lifecycle, job)
    }

    #[tokio::test]
    async fn step_closed_uses_max_of_child_span_ended_at() {
        let t0 = Utc::now() - Duration::seconds(60);
        let (lifecycle, job) = build_lifecycle_with_in_progress_job(t0).await;
        let trace: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());

        let step = pending_step(job.id, t0 + Duration::seconds(1));
        trace.save_step(&step.to_row().unwrap()).await.unwrap();

        let llm_end = t0 + Duration::seconds(20);
        let llm = make_span(
            step.id,
            llm_span_kind(),
            t0 + Duration::seconds(2),
            LifecycleState::Done(LifecycleOutcome::Ok),
            Some(llm_end),
        );
        let tool_end = t0 + Duration::seconds(35);
        let tool = make_span(
            step.id,
            tool_span_kind(),
            t0 + Duration::seconds(20),
            LifecycleState::Done(LifecycleOutcome::Cancelled {
                reason: CancelReason::ParentCancelled,
            }),
            Some(tool_end),
        );
        trace.save_span(&llm.to_row().unwrap()).await.unwrap();
        trace.save_span(&tool.to_row().unwrap()).await.unwrap();

        let summary = recover_orphaned_traces_and_jobs(trace.clone(), lifecycle.clone()).await;

        assert_eq!(summary.steps_closed, 1);
        assert_eq!(summary.spans_closed, 0);
        assert_eq!(summary.jobs_cancelled, 1);

        let reloaded = Step::from_row(trace.load_step(&step.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(reloaded.ended_at, Some(tool_end));
        assert!(matches!(
            reloaded.outcome,
            LifecycleState::Done(LifecycleOutcome::Cancelled { .. })
        ));

        let job_after = lifecycle.get(&job.id).await.unwrap().unwrap();
        assert_eq!(job_after.ended_at, Some(tool_end));
        assert!(matches!(job_after.status, JobStatus::Cancelled { .. }));
    }

    #[tokio::test]
    async fn zero_span_step_falls_back_to_started_at() {
        let t0 = Utc::now() - Duration::seconds(30);
        let (lifecycle, job) = build_lifecycle_with_in_progress_job(t0).await;
        let trace: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());

        let step_started = t0 + Duration::seconds(5);
        let step = pending_step(job.id, step_started);
        trace.save_step(&step.to_row().unwrap()).await.unwrap();

        let summary = recover_orphaned_traces_and_jobs(trace.clone(), lifecycle.clone()).await;
        assert_eq!(summary.steps_closed, 1);

        let reloaded = Step::from_row(trace.load_step(&step.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(reloaded.ended_at, Some(step_started));

        let job_after = lifecycle.get(&job.id).await.unwrap().unwrap();
        assert_eq!(job_after.ended_at, Some(step_started));
    }

    #[tokio::test]
    async fn pending_span_uses_max_event_at_then_started_at() {
        let t0 = Utc::now() - Duration::seconds(60);
        let (lifecycle, job) = build_lifecycle_with_in_progress_job(t0).await;
        let trace: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());

        let step = pending_step(job.id, t0 + Duration::seconds(1));
        trace.save_step(&step.to_row().unwrap()).await.unwrap();

        let span_started = t0 + Duration::seconds(2);
        let pending_span = make_span(
            step.id,
            llm_span_kind(),
            span_started,
            LifecycleState::Pending,
            None,
        );
        trace
            .save_span(&pending_span.to_row().unwrap())
            .await
            .unwrap();

        let ev_at = t0 + Duration::seconds(15);
        let ev = SpanEvent {
            span_id: pending_span.id,
            seq: 0,
            at: ev_at,
            kind: SpanEventKind::Approval {
                decision: ApprovalDecision::Approve,
                resource: ResourceAccess::ReadFile {
                    path: std::path::PathBuf::from("/tmp/x"),
                },
            },
        };
        trace
            .append_span_event(&ev.to_row().unwrap())
            .await
            .unwrap();

        let summary = recover_orphaned_traces_and_jobs(trace.clone(), lifecycle.clone()).await;
        assert_eq!(summary.spans_closed, 1);

        let reloaded =
            Span::from_row(trace.load_span(&pending_span.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(reloaded.ended_at, Some(ev_at));
        let step_after = Step::from_row(trace.load_step(&step.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(step_after.ended_at, Some(ev_at));
    }

    #[tokio::test]
    async fn panicked_actor_recovery_closes_pending_rows_at_crash_time() {
        let t0 = Utc::now() - Duration::seconds(60);
        let crash_at = t0 + Duration::seconds(30);
        let (lifecycle, job) = build_lifecycle_with_in_progress_job(t0).await;
        let trace: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());

        let step = pending_step(job.id, t0 + Duration::seconds(1));
        trace.save_step(&step.to_row().unwrap()).await.unwrap();
        let span = make_span(
            step.id,
            llm_span_kind(),
            t0 + Duration::seconds(2),
            LifecycleState::Pending,
            None,
        );
        trace.save_span(&span.to_row().unwrap()).await.unwrap();

        let summary = recover_panicked_actor_session(
            trace.clone(),
            lifecycle.clone(),
            &job.session_id,
            crash_at,
        )
        .await;

        assert_eq!(summary.jobs_inspected, 1);
        assert_eq!(summary.jobs_cancelled, 1);
        assert_eq!(summary.steps_closed, 1);
        assert_eq!(summary.spans_closed, 1);

        let span_after = Span::from_row(trace.load_span(&span.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(span_after.ended_at, Some(crash_at));
        assert!(matches!(
            span_after.outcome,
            LifecycleState::Done(LifecycleOutcome::Cancelled {
                reason: CancelReason::SystemCrash
            })
        ));

        let step_after = Step::from_row(trace.load_step(&step.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(step_after.ended_at, Some(crash_at));

        let job_after = lifecycle.get(&job.id).await.unwrap().unwrap();
        assert_eq!(job_after.ended_at, Some(crash_at));
        assert!(matches!(
            job_after.status,
            JobStatus::Cancelled {
                reason: CancelReason::SystemCrash,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn parallel_spans_take_max_not_last() {
        let t0 = Utc::now() - Duration::seconds(120);
        let (lifecycle, job) = build_lifecycle_with_in_progress_job(t0).await;
        let trace: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());

        let step = pending_step(job.id, t0 + Duration::seconds(1));
        trace.save_step(&step.to_row().unwrap()).await.unwrap();

        let pg = ParallelGroup::new();
        let early_end = t0 + Duration::seconds(50);
        let late_end = t0 + Duration::seconds(80);

        let mut a = make_span(
            step.id,
            tool_span_kind(),
            t0 + Duration::seconds(10),
            LifecycleState::Done(LifecycleOutcome::Ok),
            Some(late_end),
        );
        a.parallel_group = Some(pg);
        let mut b = make_span(
            step.id,
            tool_span_kind(),
            t0 + Duration::seconds(10),
            LifecycleState::Done(LifecycleOutcome::Ok),
            Some(early_end),
        );
        b.parallel_group = Some(pg);
        trace.save_span(&a.to_row().unwrap()).await.unwrap();
        trace.save_span(&b.to_row().unwrap()).await.unwrap();

        let _ = recover_orphaned_traces_and_jobs(trace.clone(), lifecycle.clone()).await;

        let step_after = Step::from_row(trace.load_step(&step.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(step_after.ended_at, Some(late_end));
    }

    #[tokio::test]
    async fn rerun_is_idempotent_on_already_terminal_rows() {
        let t0 = Utc::now() - Duration::seconds(60);
        let (lifecycle, job) = build_lifecycle_with_in_progress_job(t0).await;
        let trace: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());

        let step = pending_step(job.id, t0 + Duration::seconds(1));
        trace.save_step(&step.to_row().unwrap()).await.unwrap();

        let first = recover_orphaned_traces_and_jobs(trace.clone(), lifecycle.clone()).await;
        assert_eq!(first.jobs_cancelled, 1);
        assert_eq!(first.steps_closed, 1);

        let second = recover_orphaned_traces_and_jobs(trace.clone(), lifecycle.clone()).await;
        assert_eq!(second.jobs_inspected, 0);
        assert_eq!(second.jobs_cancelled, 0);
        assert_eq!(second.steps_closed, 0);
        assert_eq!(second.spans_closed, 0);
    }

    #[tokio::test]
    async fn pending_job_with_no_steps_is_cancelled_at_created_at() {
        let t0 = Utc::now() - Duration::seconds(45);
        let store = Arc::new(MemoryJobStore::new());
        let mut job = Job::new(
            SessionId::from("s1"),
            TriggerKind::User,
            JobInput::UserChat {
                content: vec![ContentBlock::Text("hi".into())],
            },
            None,
        );
        job.created_at = t0;
        store.create(&job.to_row().unwrap()).await.unwrap();
        let lifecycle = Arc::new(JobLifecycle::new(store));
        let trace: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());

        let summary = recover_orphaned_traces_and_jobs(trace, lifecycle.clone()).await;
        assert_eq!(summary.jobs_cancelled, 1);
        assert_eq!(summary.steps_closed, 0);

        let job_after = lifecycle.get(&job.id).await.unwrap().unwrap();
        assert_eq!(job_after.ended_at, Some(t0));
    }

    #[tokio::test]
    async fn does_not_touch_terminal_jobs() {
        let t0 = Utc::now() - Duration::seconds(30);
        let store = Arc::new(MemoryJobStore::new());
        let mut job = Job::new(
            SessionId::from("s1"),
            TriggerKind::User,
            JobInput::UserChat {
                content: vec![ContentBlock::Text("hi".into())],
            },
            None,
        );
        job.status = JobStatus::Completed;
        job.started_at = Some(t0);
        job.ended_at = Some(t0 + Duration::seconds(10));
        store.create(&job.to_row().unwrap()).await.unwrap();
        let lifecycle = Arc::new(JobLifecycle::new(store));
        let trace: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());

        let summary = recover_orphaned_traces_and_jobs(trace, lifecycle.clone()).await;
        assert_eq!(summary.jobs_inspected, 0);
        assert_eq!(summary.jobs_cancelled, 0);

        let job_after = lifecycle.get(&job.id).await.unwrap().unwrap();
        assert!(matches!(job_after.status, JobStatus::Completed));
        assert_eq!(job_after.ended_at, Some(t0 + Duration::seconds(10)));
    }

    #[tokio::test]
    async fn closes_detached_trace_rows_under_terminal_job_without_reopening_job() {
        let t0 = Utc::now() - Duration::seconds(90);
        let store = Arc::new(MemoryJobStore::new());
        let mut job = Job::new(
            SessionId::from("s1"),
            TriggerKind::User,
            JobInput::UserChat {
                content: vec![ContentBlock::Text("hi".into())],
            },
            None,
        );
        job.status = JobStatus::Completed;
        job.started_at = Some(t0);
        let job_ended = t0 + Duration::seconds(10);
        job.ended_at = Some(job_ended);
        store.create(&job.to_row().unwrap()).await.unwrap();
        let lifecycle = Arc::new(JobLifecycle::new(store));
        let trace: Arc<dyn TraceStore> = Arc::new(MemoryTraceStore::new());

        let step = Step {
            id: StepId::new(),
            job_id: job.id,
            kind: StepKind::Compression,
            started_at: t0 + Duration::seconds(20),
            ended_at: None,
            outcome: LifecycleState::Pending,
        };
        trace.save_step(&step.to_row().unwrap()).await.unwrap();
        let span = make_span(
            step.id,
            llm_span_kind(),
            t0 + Duration::seconds(21),
            LifecycleState::Pending,
            None,
        );
        trace.save_span(&span.to_row().unwrap()).await.unwrap();

        let summary = recover_orphaned_traces_and_jobs(trace.clone(), lifecycle.clone()).await;
        assert_eq!(summary.jobs_inspected, 1);
        assert_eq!(summary.jobs_cancelled, 0);
        assert_eq!(summary.steps_closed, 1);
        assert_eq!(summary.spans_closed, 1);

        let step_after = Step::from_row(trace.load_step(&step.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(step_after.ended_at, Some(span.started_at));
        assert!(matches!(
            step_after.outcome,
            LifecycleState::Done(LifecycleOutcome::Cancelled {
                reason: CancelReason::SystemCrash
            })
        ));
        let span_after = Span::from_row(trace.load_span(&span.id).await.unwrap().unwrap()).unwrap();
        assert_eq!(span_after.ended_at, Some(span.started_at));

        let job_after = lifecycle.get(&job.id).await.unwrap().unwrap();
        assert!(matches!(job_after.status, JobStatus::Completed));
        assert_eq!(job_after.ended_at, Some(job_ended));
    }
}
