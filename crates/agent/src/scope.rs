//! Scope-guard combinators for trace lifecycles.
//!
//! `begin_*` / `end_*` pairs that are written imperatively are
//! exception-unsafe in the `?`-propagation sense: any `?` between the
//! `begin` and the `end` short-circuits the close and leaves a
//! half-open row in storage. These helpers wrap each pair in a
//! `match`, so the body can use `?` freely while the close still runs
//! on both arms.
//!
//! ## Cancellation
//!
//! `LifecycleOutcome::Cancelled { reason }` and `Failed { reason }`
//! are distinct terminal states with distinct downstream semantics
//! (replay / cost-attribution UIs treat them differently — see
//! `aura_trace::outcome` and `aura_job::CancelReason`). Callers that
//! run inside a cancellable scope pass `Some((token, reason))`; on
//! `Err` the helper checks `token.is_cancelled()` and records the
//! body's exit as `Cancelled { reason }` instead of `Failed`. Callers
//! without a token pass `None` and every `Err` is `Failed`.
//!
//! Bodies that genuinely *complete* their work (return `Ok`) record
//! `Ok` even if the token was tripped after body return — completion
//! beat the cancel; the body produced a real result and we should not
//! discard it. Bodies that observe the token and want to bail mid-flight
//! should return `Err`, e.g. `bail!("cancelled")` — the helper turns
//! that into the right `Cancelled` outcome.
//!
//! Errors emitted by the close call itself are logged at `warn` and
//! swallowed: the body's outcome is what propagates.

use std::future::Future;

use aura_job::{CancelReason, JobInput, JobLifecycle, JobOutput};
use aura_model::{JobId, ParallelGroup, SessionId, TriggerKind};
use aura_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallResult, SpanFinalize, SpanHandle, SpanKind, StepHandle,
    StepKind,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::trace::SpanRecorder;

/// Inputs needed to create a new `Job`. Bundled into a struct so
/// [`with_job`] can own the full `start_job → start → body → complete/fail`
/// lifecycle without ballooning its parameter list — production callers
/// must go through [`with_job`], not `JobLifecycle::start_job` directly,
/// so the cancel state machine can't be skipped.
pub(crate) struct JobSpec {
    pub session_id: SessionId,
    pub session_trigger_kind: TriggerKind,
    pub input: JobInput,
    pub effective_soul_version: String,
    pub parent_job_id: Option<JobId>,
}

/// Optional cancel context. When the body returns `Err` and the token
/// is tripped, the resource closes as `Cancelled { reason }` rather
/// than `Failed`.
pub(crate) type CancelContext<'a> = Option<(&'a CancellationToken, CancelReason)>;

/// Map a body `Err` to its terminal trace outcome: `Cancelled` if the
/// scope's cancel token has been tripped, `Failed { reason: e.to_string() }`
/// otherwise. Body `Ok` is handled directly by callers (which now carry
/// the explicit `LifecycleOutcome`), so this only fires on the error
/// path.
fn outcome_on_err<E: std::fmt::Display>(e: &E, cancel: CancelContext<'_>) -> LifecycleOutcome {
    match cancel {
        Some((token, reason)) if token.is_cancelled() => LifecycleOutcome::Cancelled { reason },
        _ => LifecycleOutcome::Failed {
            reason: e.to_string(),
        },
    }
}

/// Open a `Step`, run `body`, close the step.
///
/// Body returns `(LifecycleOutcome, T)` so the success path can record
/// a non-`Ok` outcome too — a subagent stub that returned successfully
/// but with a `Cancelled` status maps that into the step's terminal
/// state without faking an `Err`. For the common case `(Ok, value)`
/// just write `Ok((LifecycleOutcome::Ok, value))`.
///
/// On body `Err` the helper closes as `Cancelled { reason }` when
/// `cancel` is `Some` and the token has been tripped, otherwise
/// `Failed { reason: e.to_string() }`.
pub(crate) async fn with_step<F, Fut, T>(
    rec: &SpanRecorder,
    job_id: JobId,
    kind: StepKind,
    cancel: CancelContext<'_>,
    body: F,
) -> anyhow::Result<T>
where
    F: FnOnce(StepHandle) -> Fut,
    Fut: Future<Output = anyhow::Result<(LifecycleOutcome, T)>>,
{
    let step = rec.begin_step(job_id, kind).await?;
    let result = body(step.clone()).await;
    let outcome = match &result {
        Ok((o, _)) => o.clone(),
        Err(e) => outcome_on_err(e, cancel),
    };
    if let Err(close_err) = rec.end_step(step, outcome).await {
        match &result {
            Ok(_) => warn!(error = %close_err, "end_step failed on success path"),
            Err(orig) => {
                warn!(error = %close_err, original = %orig, "end_step failed on error path")
            }
        }
    }
    result.map(|(_, v)| v)
}

/// Own a full `Job` lifecycle: create the row via `start_job`,
/// register the cancellation token, transition `Pending → InProgress`,
/// run the body, then `complete` on success or `fail` on error.
///
/// Production callers must go through this helper rather than calling
/// `JobLifecycle::start_job` directly, so the cancel state machine
/// (token registry + InProgress transition + cancel-race windows
/// below) can't be skipped. Tests inside the agent crate may still
/// use `start_job` directly to construct fixtures in arbitrary states.
///
/// Body returns `(JobOutput, T)` on success: the helper writes the
/// output via `complete`, and `T` flows back to the caller.
/// `JobLifecycle::complete` / `fail` / `cancel` themselves publish
/// terminal events on the broadcast bus; this helper does not need
/// to fire any extra notification.
///
/// ## Cancel-race handling
///
/// `JobLifecycle::cancel` first trips the registered token, *then*
/// flips the row to `Cancelled`. Three windows are handled:
/// 1. cancel arrives before `start()` → start() succeeds (Pending →
///    InProgress is allowed even on a cancelled token; the body's
///    own cancel observation kicks in next).
/// 2. cancel arrives during body → body returns Err, helper checks
///    `cancel_token.is_cancelled()`, and skips `fail()` because the
///    row is already Cancelled (calling fail() would log noise as
///    InvalidTransition).
/// 3. cancel arrives between body returning Ok and `complete()`'s
///    write → complete() returns InvalidTransition; helper turns
///    that into Err so the caller doesn't dispatch the response.
pub(crate) async fn with_job<F, Fut, T>(
    lifecycle: &JobLifecycle,
    cancel_token: CancellationToken,
    spec: JobSpec,
    body: F,
) -> anyhow::Result<T>
where
    F: FnOnce(JobId) -> Fut,
    Fut: Future<Output = anyhow::Result<(JobOutput, T)>>,
{
    let job = lifecycle
        .start_job(
            spec.session_id,
            spec.session_trigger_kind,
            spec.input,
            spec.effective_soul_version,
            spec.parent_job_id,
        )
        .await?;
    let job_id = job.id;
    let _cancel_guard = lifecycle.register_running(job_id, cancel_token.clone());

    if let Err(e) = lifecycle.start(&job_id).await {
        // Pending row exists from start_job; mark Failed so it doesn't
        // leak as forever-Pending.
        lifecycle
            .fail(&job_id, format!("start failed: {e}"))
            .await
            .ok();
        return Err(e.into());
    }

    let result = body(job_id).await;

    match result {
        Ok((output, value)) => {
            if cancel_token.is_cancelled() {
                warn!(job_id = %job_id, "cancel observed after body returned Ok; suppressing complete");
                return Err(anyhow::anyhow!("job cancelled mid-flight"));
            }
            match lifecycle.complete(&job_id, output).await {
                Ok(()) => Ok(value),
                Err(e) => {
                    warn!(error = %e, job_id = %job_id, "complete() rejected; treating as cancelled");
                    Err(anyhow::anyhow!("job already terminal: {e}"))
                }
            }
        }
        Err(e) => {
            if cancel_token.is_cancelled() {
                // Row already Cancelled. Don't call fail() — it would
                // return InvalidTransition.
                return Err(e);
            }
            if let Err(fe) = lifecycle.fail(&job_id, e.to_string()).await {
                warn!(error = %fe, "failed to mark job failed");
            }
            Err(e)
        }
    }
}

/// Open a `Span`, run `body`, close the span.
///
/// Body returns `(SpanFinalize, LifecycleOutcome, T)` so the success
/// path can record a non-`Ok` outcome too — a tool whose
/// [`aura_tools::ToolOutput::Error`] arm should land as
/// `Failed { reason }` in the trace, or a subagent stub that mapped a
/// `Cancelled` status, both ride this single shape. For the common
/// case `(Ok, value)` just write `Ok((finalize, LifecycleOutcome::Ok, value))`.
///
/// On body `Err` the helper closes via `end_span` with
/// `SpanFinalize::Empty` (no end-time payload available) and
/// `Cancelled { reason }` / `Failed { reason: e.to_string() }` per the
/// same rules as `with_step`.
pub(crate) async fn with_span<F, Fut, T>(
    rec: &SpanRecorder,
    step: &StepHandle,
    job_id: JobId,
    kind: SpanKind,
    parallel_group: Option<ParallelGroup>,
    cancel: CancelContext<'_>,
    body: F,
) -> anyhow::Result<T>
where
    F: FnOnce(SpanHandle) -> Fut,
    Fut: Future<Output = anyhow::Result<(SpanFinalize, LifecycleOutcome, T)>>,
{
    let span = rec.begin_span(step, kind, parallel_group).await?;
    let result = body(span.clone()).await;
    let (finalize, outcome, value_result): (SpanFinalize, LifecycleOutcome, anyhow::Result<T>) =
        match result {
            Ok((f, o, v)) => (f, o, Ok(v)),
            Err(e) => {
                let outcome = outcome_on_err(&e, cancel);
                (SpanFinalize::Empty, outcome, Err(e))
            }
        };
    if let Err(close_err) = rec.end_span(span, job_id, finalize, outcome).await {
        match &value_result {
            Ok(_) => warn!(error = %close_err, "end_span failed on success path"),
            Err(orig) => {
                warn!(error = %close_err, original = %orig, "end_span failed on error path")
            }
        }
    }
    value_result
}

/// LLM-call-aware variant of [`with_span`]: the body must always
/// produce an [`LlmCallResult`] alongside the success-or-error result,
/// so the resulting `LlmSpanEnded` event fires on **every** terminal
/// path — including provider errors, sanitize failures, mid-stream
/// drops, and cancellations.
///
/// `with_span`'s "Err → SpanFinalize::Empty" rule is the right
/// behaviour for tool calls (no token economy attached) but for LLM
/// calls it silently drops billing for any failed attempt that already
/// consumed input tokens or streamed partial output. This helper
/// closes that hole: callers compute a best-effort [`LlmCallResult`]
/// (zero tokens when nothing was observed, partial counts when the
/// stream got some way through) and the helper guarantees the
/// `cost_records` row lands.
///
/// Cancel semantics mirror [`with_span`].
pub(crate) async fn with_llm_span<F, Fut, T>(
    rec: &SpanRecorder,
    step: &StepHandle,
    job_id: JobId,
    begin: LlmCallBegin,
    cancel: CancelContext<'_>,
    body: F,
) -> anyhow::Result<T>
where
    F: FnOnce(SpanHandle) -> Fut,
    Fut: Future<Output = (LlmCallResult, anyhow::Result<T>)>,
{
    let span = rec
        .begin_span(
            step,
            SpanKind::LlmCall {
                begin,
                result: None,
            },
            None,
        )
        .await?;
    let (call_result, value_result) = body(span.clone()).await;
    let outcome = match &value_result {
        Ok(_) => LifecycleOutcome::Ok,
        Err(e) => outcome_on_err(e, cancel),
    };
    let finalize = SpanFinalize::LlmCall(call_result);
    if let Err(close_err) = rec.end_span(span, job_id, finalize, outcome).await {
        match &value_result {
            Ok(_) => warn!(error = %close_err, "end_llm_span failed on success path"),
            Err(orig) => {
                warn!(error = %close_err, original = %orig, "end_llm_span failed on error path")
            }
        }
    }
    value_result
}
