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

use aura_job::{CancelReason, JobOutput};
use aura_model::{JobId, ParallelGroup};
use aura_trace::{LifecycleOutcome, SpanFinalize, SpanHandle, SpanKind, StepHandle, StepKind};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::job::JobLifecycle;
use crate::trace::SpanRecorder;

/// Optional cancel context. When the body returns `Err` and the token
/// is tripped, the resource closes as `Cancelled { reason }` rather
/// than `Failed`.
pub(crate) type CancelContext<'a> = Option<(&'a CancellationToken, CancelReason)>;

fn outcome_for<T, E: std::fmt::Display>(
    res: Result<&T, &E>,
    cancel: CancelContext<'_>,
) -> LifecycleOutcome {
    match res {
        Ok(_) => LifecycleOutcome::Ok,
        Err(e) => match cancel {
            Some((token, reason)) if token.is_cancelled() => {
                LifecycleOutcome::Cancelled { reason }
            }
            _ => LifecycleOutcome::Failed {
                reason: e.to_string(),
            },
        },
    }
}

/// Open a `Step`, run `body`, close the step.
///
/// Closes with `Ok` on body success, `Cancelled { reason }` on body
/// `Err` when `cancel` is `Some` and the token has been tripped, and
/// `Failed { reason }` otherwise.
pub(crate) async fn with_step<F, Fut, T>(
    rec: &SpanRecorder,
    job_id: JobId,
    kind: StepKind,
    cancel: CancelContext<'_>,
    body: F,
) -> anyhow::Result<T>
where
    F: FnOnce(StepHandle) -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let step = rec.begin_step(job_id, kind).await?;
    let result = body(step.clone()).await;
    let outcome = outcome_for(result.as_ref(), cancel);
    if let Err(close_err) = rec.end_step(step, outcome).await {
        match &result {
            Ok(_) => warn!(error = %close_err, "end_step failed on success path"),
            Err(orig) => warn!(error = %close_err, original = %orig, "end_step failed on error path"),
        }
    }
    result
}

/// Wrap the in-flight portion of a `Job` lifecycle: registers the
/// cancellation token, transitions `Pending → InProgress`, runs the
/// body, then `complete`s on success or `fail`s on error. The caller
/// owns `start_job` — it returns the `JobId` they pass in here, and
/// keeping it outside the helper means the caller still has the id
/// available on the `Err` path (e.g. to dispatch a follow-up
/// diagnostic job whose `parent_job_id` points at the failed job).
///
/// Body returns `(JobOutput, T)` on success: the helper writes the
/// output via `complete`, and `T` flows back to the caller. The
/// terminal `JobCompleted` broadcast is **not** emitted here — that
/// is per-actor concern and lives at the call site (e.g.
/// `AgentActor::run_loop_with_terminal_emit`, or the cron path's
/// success arm).
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
    job_id: JobId,
    body: F,
) -> anyhow::Result<T>
where
    F: FnOnce(JobId) -> Fut,
    Fut: Future<Output = anyhow::Result<(JobOutput, T)>>,
{
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
/// On `Ok` the body returns `(SpanFinalize, T)`; the helper passes the
/// finalize payload to `end_span` with `LifecycleOutcome::Ok`. On
/// `Err` the helper closes via `end_span` with `SpanFinalize::Empty`
/// (no end-time payload available) and `Cancelled { reason }` /
/// `Failed { reason }` per the same rules as `with_step`.
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
    Fut: Future<Output = anyhow::Result<(SpanFinalize, T)>>,
{
    let span = rec.begin_span(step, kind, parallel_group).await?;
    let result = body(span.clone()).await;
    let (finalize, value_result): (SpanFinalize, anyhow::Result<T>) = match result {
        Ok((f, v)) => (f, Ok(v)),
        Err(e) => (SpanFinalize::Empty, Err(e)),
    };
    let outcome = outcome_for(value_result.as_ref(), cancel);
    if let Err(close_err) = rec.end_span(span, job_id, finalize, outcome).await {
        match &value_result {
            Ok(_) => warn!(error = %close_err, "end_span failed on success path"),
            Err(orig) => warn!(error = %close_err, original = %orig, "end_span failed on error path"),
        }
    }
    value_result
}
