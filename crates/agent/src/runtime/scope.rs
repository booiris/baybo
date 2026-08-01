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
//! `baybo_trace::outcome` and `baybo_turn::CancelReason`). Callers that
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

use baybo_model::{ParallelGroup, SessionId, TriggerKind, TurnId};
use baybo_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallResult, SpanFinalize, SpanHandle, SpanKind,
    SpanRecorder, StepHandle, StepKind,
};
use baybo_turn::{CancelReason, TurnInput, TurnLifecycle, TurnOutput};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Inputs needed to create a new `Turn`. Bundled into a struct so
/// [`with_turn`] can own the full `start_turn → start → body → complete/fail`
/// lifecycle without ballooning its parameter list — production callers
/// must go through [`with_turn`], not `TurnLifecycle::start_turn` directly,
/// so the cancel state machine can't be skipped.
pub(crate) struct TurnSpec {
    pub session_id: SessionId,
    /// The owning session's root trigger — recorded on the turn as its
    /// `origin`, independent of `input`.
    pub origin: TriggerKind,
    pub input: TurnInput,
    pub parent_turn_id: Option<TurnId>,
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
    turn_id: TurnId,
    kind: StepKind,
    cancel: CancelContext<'_>,
    body: F,
) -> anyhow::Result<T>
where
    F: FnOnce(StepHandle) -> Fut,
    Fut: Future<Output = anyhow::Result<(LifecycleOutcome, T)>>,
{
    let step = rec.begin_step(turn_id, kind).await?;
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

/// Own a full `Turn` lifecycle: create the row via `start_turn`,
/// register the cancellation token, transition `Pending → InProgress`,
/// run the body, then settle the row exactly once.
///
/// Production callers must go through this helper rather than calling
/// `TurnLifecycle::start_turn` directly, so the cancel state machine
/// (token registry + InProgress transition + cancel-race windows
/// below) can't be skipped. Tests inside the agent crate may still
/// use `start_turn` directly to construct fixtures in arbitrary states.
///
/// Body returns `(TurnOutput, T)` on success: the helper writes the
/// output via `complete`, and `T` flows back to the caller.
/// `TurnLifecycle::complete` / `fail` / `cancel` themselves publish
/// terminal events on the broadcast bus; this helper does not need
/// to fire any extra notification.
///
/// ## One settlement, one write
///
/// Past `start()` there is no `return`: the body's result and the token
/// classify into a [`TurnSettlement`], and [`settle_turn`] is the only
/// thing that writes a terminal state — the same shape as this module's
/// other three guards. That is structural, not a convention: a new case
/// has nowhere to exit from except by naming a settlement, so it cannot
/// leave the row `InProgress` the way an early `return` once did. A row
/// that leaks non-terminal is not merely untidy — a `/stop`, a shutdown,
/// and the dream pass all read "is this session mid-turn?" from it.
///
/// ## Cancel-race handling
///
/// `TurnLifecycle::cancel` first trips the registered token, *then*
/// flips the row to `Cancelled`, and the token can also be tripped by
/// something that never touches this row at all (shutdown's parent
/// token, `/stop` reaching a child's pre-dispatch token). Four windows:
/// 1. cancel arrives before `start()` → start() succeeds (Pending →
///    InProgress is allowed even on a cancelled token; the body's
///    own cancel observation kicks in next).
/// 2. cancel arrives during body → body returns `Err`, and the
///    settlement is `Cancel`.
/// 3. cancel arrives while the body is finishing → body returns `Ok`,
///    the token reads tripped, and the settlement is still `Cancel`:
///    the value is being discarded, so the row must not claim the turn
///    delivered anything.
/// 4. cancel lands between that check and `complete()`'s write →
///    `complete()` is rejected, and the settlement falls back to
///    cancelling the row rather than leaving it running.
pub(crate) async fn with_turn<F, Fut, T>(
    lifecycle: &TurnLifecycle,
    cancel_token: CancellationToken,
    spec: TurnSpec,
    body: F,
) -> anyhow::Result<T>
where
    F: FnOnce(TurnId) -> Fut,
    Fut: Future<Output = anyhow::Result<(TurnOutput, T)>>,
{
    let turn = lifecycle
        .start_turn(
            spec.session_id,
            spec.origin,
            spec.input,
            spec.parent_turn_id,
        )
        .await?;
    let turn_id = turn.id;
    let _cancel_guard = lifecycle.register_running(turn_id, cancel_token.clone());

    if let Err(e) = lifecycle.start(&turn_id).await {
        // Pending row exists from start_turn; mark Failed so it doesn't
        // leak as forever-Pending.
        lifecycle
            .fail(&turn_id, format!("start failed: {e}"))
            .await
            .ok();
        return Err(e.into());
    }

    let settlement = match (body(turn_id).await, cancel_token.is_cancelled()) {
        (Ok((output, value)), false) => TurnSettlement::Complete(output, value),
        (Ok(_), true) => {
            warn!(turn_id = %turn_id, "cancel observed after body returned Ok; suppressing complete");
            TurnSettlement::Cancel(anyhow::anyhow!("turn cancelled mid-flight"))
        }
        (Err(e), true) => TurnSettlement::Cancel(e),
        (Err(e), false) => TurnSettlement::Fail(e),
    };
    settle_turn(lifecycle, turn_id, settlement).await
}

/// How a finished body settles its turn row. Every exit from
/// [`with_turn`] past `start()` is one of these, so "which terminal
/// state" is decided in one place and written in one place.
enum TurnSettlement<T> {
    Complete(TurnOutput, T),
    /// The row settles as `Cancelled`, and the caller gets this error
    /// instead of a value.
    Cancel(anyhow::Error),
    Fail(anyhow::Error),
}

/// The single terminal write for a turn.
async fn settle_turn<T>(
    lifecycle: &TurnLifecycle,
    turn_id: TurnId,
    settlement: TurnSettlement<T>,
) -> anyhow::Result<T> {
    match settlement {
        TurnSettlement::Complete(output, value) => match lifecycle.complete(&turn_id, output).await
        {
            Ok(()) => Ok(value),
            Err(e) => {
                warn!(error = %e, turn_id = %turn_id, "complete() rejected; treating as cancelled");
                cancel_row(lifecycle, turn_id).await;
                Err(anyhow::anyhow!("turn already terminal: {e}"))
            }
        },
        TurnSettlement::Cancel(e) => {
            cancel_row(lifecycle, turn_id).await;
            Err(e)
        }
        TurnSettlement::Fail(e) => {
            if let Err(fe) = lifecycle.fail(&turn_id, e.to_string()).await {
                warn!(error = %fe, turn_id = %turn_id, "failed to mark turn failed");
            }
            Err(e)
        }
    }
}

/// Flip the row to `Cancelled`, tolerating a row that is already
/// terminal.
///
/// `cancel` is the right verb even when the row might still be
/// `InProgress`: it is idempotent on a terminal row (preserving the
/// original canceller's reason) and flips a running one, whereas `fail`
/// would be rejected as an invalid transition on the terminal case and
/// log noise for it.
async fn cancel_row(lifecycle: &TurnLifecycle, turn_id: TurnId) {
    if let Err(e) = lifecycle
        .cancel(&turn_id, CancelReason::ParentCancelled, Vec::new())
        .await
    {
        warn!(error = %e, turn_id = %turn_id, "failed to settle turn as cancelled");
    }
}

/// Open a `Span`, run `body`, close the span.
///
/// Body returns `(SpanFinalize, LifecycleOutcome, T)` so the success
/// path can record a non-`Ok` outcome too — a tool whose
/// [`baybo_tools::ToolOutput::Error`] arm should land as
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
    turn_id: TurnId,
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
    if let Err(close_err) = rec.end_span(span, turn_id, finalize, outcome).await {
        match &value_result {
            Ok(_) => warn!(error = %close_err, "end_span failed on success path"),
            Err(orig) => {
                warn!(error = %close_err, original = %orig, "end_span failed on error path")
            }
        }
    }
    value_result
}

/// LLM-call span guard: the body must always
/// produce an [`LlmCallResult`] alongside the success-or-error result,
/// so the resulting `LlmSpanEnded` event fires on **every** terminal
/// path — including provider errors, sanitize failures, mid-stream
/// drops, and cancellations.
///
/// A generic "Err → SpanFinalize::Empty" rule would silently drop billing
/// for an LLM attempt that already
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
    turn_id: TurnId,
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
    if let Err(close_err) = rec.end_span(span, turn_id, finalize, outcome).await {
        match &value_result {
            Ok(_) => warn!(error = %close_err, "end_llm_span failed on success path"),
            Err(orig) => {
                warn!(error = %close_err, original = %orig, "end_llm_span failed on error path")
            }
        }
    }
    value_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_store::TurnStore;
    use baybo_turn::test_support::MemoryTurnStore;
    use std::sync::Arc;

    fn spec() -> TurnSpec {
        TurnSpec {
            session_id: SessionId::from("sess"),
            origin: TriggerKind::User,
            input: TurnInput::UserChat {
                content: Vec::new(),
            },
            parent_turn_id: None,
        }
    }

    /// The row's status after the guard returns, whatever it returned.
    async fn settled_status(
        store: &Arc<MemoryTurnStore>,
        lifecycle: &TurnLifecycle,
        token: CancellationToken,
        body: anyhow::Result<TurnOutput>,
    ) -> String {
        let _ = with_turn(lifecycle, token, spec(), |_| async move {
            body.map(|output| (output, ()))
        })
        .await;
        let rows = store
            .list_by_session(&SessionId::from("sess"))
            .await
            .expect("list");
        rows.last().expect("a turn row").status_kind.clone()
    }

    fn done() -> TurnOutput {
        TurnOutput::Structured {
            value: serde_json::Value::Null,
        }
    }

    /// The regression this shape exists for. A token tripped by something
    /// that never touches this row — shutdown's parent token, `/stop`
    /// reaching a child — used to make the guard return early with no
    /// terminal write at all, leaving the row `in_progress` for the rest of
    /// the process. `/stop`, shutdown and the dream pass all read that
    /// column to answer "is this session mid-turn?", so the row is not
    /// bookkeeping — a leak silently freezes each of them on that session.
    #[tokio::test]
    async fn a_body_that_succeeds_under_an_externally_tripped_token_still_settles() {
        let store = Arc::new(MemoryTurnStore::new());
        let lifecycle = TurnLifecycle::new(Arc::clone(&store) as Arc<dyn TurnStore>);
        // Tripped without going through `TurnLifecycle::cancel`, so nothing
        // else is going to flip the row on our behalf.
        let token = CancellationToken::new();
        token.cancel();

        let status = settled_status(&store, &lifecycle, token, Ok(done())).await;
        assert_eq!(status, "cancelled", "the row must not be left running");
    }

    #[tokio::test]
    async fn every_other_exit_settles_too() {
        for (body, cancelled, expected) in [
            (Ok(done()), false, "completed"),
            (Err(anyhow::anyhow!("boom")), false, "failed"),
            (Err(anyhow::anyhow!("boom")), true, "cancelled"),
        ] {
            let store = Arc::new(MemoryTurnStore::new());
            let lifecycle = TurnLifecycle::new(Arc::clone(&store) as Arc<dyn TurnStore>);
            let token = CancellationToken::new();
            if cancelled {
                token.cancel();
            }
            let status = settled_status(&store, &lifecycle, token, body).await;
            assert_eq!(status, expected, "cancelled={cancelled}");
        }
    }

    /// The value only survives the path where the turn actually completed —
    /// a suppressed result must not reach the caller as success.
    #[tokio::test]
    async fn only_a_completed_turn_hands_its_value_back() {
        let store = Arc::new(MemoryTurnStore::new());
        let lifecycle = TurnLifecycle::new(Arc::clone(&store) as Arc<dyn TurnStore>);
        let token = CancellationToken::new();
        let ok = with_turn(&lifecycle, token.clone(), spec(), |_| async {
            Ok((done(), 42u8))
        })
        .await;
        assert_eq!(ok.expect("completed"), 42);

        token.cancel();
        let suppressed =
            with_turn(&lifecycle, token, spec(), |_| async { Ok((done(), 42u8)) }).await;
        assert!(suppressed.is_err(), "a cancelled turn hands back no value");
    }
}
