//! The progress-observer LLM call: a read-only, out-of-band summary of
//! the in-flight turn, shipped to the user as an `AgentEvent::Notice` on
//! a long UserChat turn. It is the user-facing twin of
//! [`crate::runtime::compression::CompressionRunner`] and reuses the same
//! step, span, attribution, and sanitize machinery, but it never writes
//! back to the turn's context. The agent loop fires it at an iteration
//! boundary (so the context snapshot is coherent) and emits the returned
//! line.

use std::sync::Arc;

use aura_llm::{Attribution, BillableLlm, ChatRequest, ModelInfo};
use aura_model::{JobId, SessionId};
use aura_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallInputs, LlmCallResult, SpanRecorder, StepKind,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::security::SecurityGateway;

/// How long a UserChat turn must run (with >1 iteration) before the
/// observer first fires. Short turns never trigger it; the mechanical
/// status condenser (PR #58) covers the sub-threshold window.
pub(crate) const OBSERVER_APPEAR_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// Minimum gap between successive observer Notices on one turn. Each tick
/// is a billed LLM sub-call, so it stays sparse — milestones, not a heartbeat.
pub(crate) const OBSERVER_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(40);

/// Appended as a user turn to the live context. Asks for a one-line,
/// user-facing status — never the final answer.
pub(crate) const PROGRESS_OBSERVER_PROMPT: &str = r#"You are observing an assistant that is still working on the user's request. In ONE short sentence, in the user's own language, tell the user what has been done so far and what is happening right now — a brief progress update, not the final answer. Output only that one line: no preamble, no markdown, no quotes."#;

/// Pure gate (everything except the live cancel check, which the caller
/// does separately). Extracted so the streaming / user / iteration /
/// appear-after / throttle logic is unit-testable without an LLM or real
/// wall-clock waits.
pub(crate) fn should_fire_observer(
    is_streaming: bool,
    trigger_is_user: bool,
    iterations: usize,
    turn_started: std::time::Instant,
    last_observer_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    is_streaming
        && trigger_is_user
        // iteration 1 is the original turn; ≥1 tool round must have run
        // for there to be progress worth summarizing.
        && iterations > 1
        && now.duration_since(turn_started) >= OBSERVER_APPEAR_AFTER
        && last_observer_at.is_none_or(|last| now.duration_since(last) >= OBSERVER_MIN_INTERVAL)
}

/// Agent-side dependencies for one progress-observer LLM call — the same
/// identity / cancel context that pin a normal turn call, minus anything
/// that mutates state.
pub(crate) struct ProgressObserverRunner {
    pub(crate) llm_client: Arc<BillableLlm>,
    pub(crate) recorder: Arc<SpanRecorder>,
    /// Same gateway as the main LLM path. The observer reads the *raw*
    /// in-flight context (tool results that may carry secret-like text),
    /// so its summary is scrubbed through the gateway before it leaves
    /// the agent as a user-visible Notice — otherwise the observer would
    /// bypass the `Message`-only egress sanitizer in `actor::router`.
    pub(crate) security_gateway: Arc<SecurityGateway>,
    pub(crate) job_id: JobId,
    pub(crate) user_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) model_info: ModelInfo,
    pub(crate) cancel_token: CancellationToken,
}

impl ProgressObserverRunner {
    /// Run the observer call over `request` (the live context + a
    /// "summarize progress" instruction, no tools). Brackets it in a
    /// `StepKind::ProgressObserver` step + `LlmCall` span so cost + trace
    /// attribute to the turn's job, sanitizes the response, and returns
    /// the trimmed summary text (empty string when the model produced
    /// nothing — the caller then emits no Notice).
    pub(crate) async fn run(self, request: ChatRequest) -> anyhow::Result<String> {
        let ProgressObserverRunner {
            llm_client,
            recorder,
            security_gateway,
            job_id,
            user_id,
            session_id,
            model_info,
            cancel_token,
        } = self;

        let cancel_ctx = Some((&cancel_token, aura_job::CancelReason::ParentCancelled));
        let begin = LlmCallBegin {
            model_id: model_info.id.clone(),
            provider: model_info.provider.clone(),
            provider_config_hash: String::new(),
            // One-off slice built off the live transcript — it never lands
            // in `session_messages`, so embed inline.
            input_messages: LlmCallInputs::Inline(request.messages.clone()),
            temperature: request.temperature,
        };

        let recorder_inner = Arc::clone(&recorder);
        crate::runtime::scope::with_step(
            recorder.as_ref(),
            job_id,
            StepKind::ProgressObserver,
            cancel_ctx,
            |step| async move {
                let text = crate::runtime::scope::with_llm_span(
                    recorder_inner.as_ref(),
                    &step,
                    job_id,
                    begin,
                    cancel_ctx,
                    |span| async move {
                        let bound = llm_client.bind(Attribution {
                            user_id: user_id.clone(),
                            session_id: session_id.clone(),
                            job_id,
                            span_id: span.span_id,
                        });
                        match bound.chat(&request).await {
                            Ok(billed) => {
                                let mut response = billed.response;
                                if let Err(e) =
                                    security_gateway.sanitize_llm_response(&mut response).await
                                {
                                    warn!(error = %e, "progress observer: sanitize_llm_response failed");
                                }
                                let call_result = LlmCallResult {
                                    output_content: response.content.clone(),
                                    thinking: response.thinking.clone(),
                                    tool_calls: vec![],
                                    input_tokens: response.usage.input_tokens,
                                    output_tokens: response.usage.output_tokens,
                                    cached_input_tokens: response.usage.cached_input_tokens,
                                    cache_creation_input_tokens: response
                                        .usage
                                        .cache_creation_input_tokens,
                                };
                                (call_result, Ok(response.content.trim().to_string()))
                            }
                            Err(e) => {
                                let raw = e.to_string();
                                let msg =
                                    security_gateway.sanitize_error(&raw).await.unwrap_or(raw);
                                (LlmCallResult::default(), Err(anyhow::anyhow!(msg)))
                            }
                        }
                    },
                )
                .await?;
                Ok((LifecycleOutcome::Ok, text))
            },
        )
        .await
    }
}

#[cfg(test)]
mod gate_tests {
    //! `should_fire_observer` is the pure throttle / appear gate. Instants
    //! are built relative to a base so the timing logic is tested without
    //! real wall-clock waits.
    use super::{OBSERVER_APPEAR_AFTER, OBSERVER_MIN_INTERVAL, should_fire_observer};
    use std::time::{Duration, Instant};

    #[test]
    fn fires_after_appear_with_no_prior_notice() {
        let t0 = Instant::now();
        assert!(should_fire_observer(
            true,
            true,
            2,
            t0,
            None,
            t0 + OBSERVER_APPEAR_AFTER
        ));
    }

    #[test]
    fn never_fires_before_appear_threshold() {
        let t0 = Instant::now();
        assert!(!should_fire_observer(
            true,
            true,
            5,
            t0,
            None,
            t0 + Duration::from_secs(1)
        ));
    }

    #[test]
    fn never_fires_on_the_first_iteration() {
        let t0 = Instant::now();
        assert!(!should_fire_observer(
            true,
            true,
            1,
            t0,
            None,
            t0 + OBSERVER_APPEAR_AFTER
        ));
    }

    #[test]
    fn never_fires_for_non_user_trigger() {
        let t0 = Instant::now();
        assert!(!should_fire_observer(
            true,
            false,
            5,
            t0,
            None,
            t0 + OBSERVER_APPEAR_AFTER
        ));
    }

    #[test]
    fn never_fires_when_not_streaming() {
        let t0 = Instant::now();
        assert!(!should_fire_observer(
            false,
            true,
            5,
            t0,
            None,
            t0 + OBSERVER_APPEAR_AFTER
        ));
    }

    #[test]
    fn throttles_within_min_interval_of_last_notice() {
        let t0 = Instant::now();
        let now = t0 + OBSERVER_APPEAR_AFTER + OBSERVER_MIN_INTERVAL;
        // A notice just went out at `now` → the next one is suppressed.
        assert!(!should_fire_observer(true, true, 9, t0, Some(now), now));
    }

    #[test]
    fn fires_again_once_min_interval_elapses() {
        let t0 = Instant::now();
        let last = t0 + OBSERVER_APPEAR_AFTER;
        let now = last + OBSERVER_MIN_INTERVAL;
        assert!(should_fire_observer(true, true, 9, t0, Some(last), now));
    }
}
