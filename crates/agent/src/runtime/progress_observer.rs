//! The progress-observer LLM call: a read-only, out-of-band summary of
//! the in-flight turn, shipped to the user as a transient
//! `AgentEvent::Progress` on a long UserChat turn. It is the user-facing twin of
//! [`crate::runtime::compression::CompressionRunner`] and reuses the same
//! step, span, attribution, and sanitize machinery, but it never writes
//! back to the turn's context. The agent loop fires it at an iteration
//! boundary (so the context snapshot is coherent) and emits the returned
//! line.

use std::sync::Arc;

use baybo_llm::{Attribution, BillableLlm, ChatRequest, ModelInfo};
use baybo_model::{ChannelType, JobId, SessionId};
use baybo_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallInputs, LlmCallResult, SpanRecorder, StepKind,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::security::SecurityGateway;

/// How long a UserChat turn must run (with >1 iteration) before the
/// observer first fires. Short turns never trigger it; the mechanical
/// status condenser (PR #58) covers the sub-threshold window.
pub(crate) const OBSERVER_APPEAR_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// Minimum gap between successive observer updates on one turn. Each tick
/// is a billed LLM sub-call, so it stays sparse — milestones, not a heartbeat.
pub(crate) const OBSERVER_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(40);

/// Appended as a user turn to the live context. Asks for a one-line,
/// user-facing status — never the final answer.
pub(crate) const PROGRESS_OBSERVER_PROMPT: &str = r#"You are observing an assistant that is still working on the user's request. In ONE short sentence, in the user's own language, tell the user what has been done so far and what is happening right now — a brief progress update, not the final answer. Output only that one line: no preamble, no markdown, no quotes."#;

/// Prepended to [`PROGRESS_OBSERVER_PROMPT`] once the turn has already shown
/// the user one or more progress lines, so the model sees what it said and
/// advances instead of repeating. `{{prior_notices}}` expands to the
/// newline-joined list (oldest first). This whole block is the appended
/// suffix; it never edits the cached `messages_for_llm()` prefix.
const PROGRESS_OBSERVER_PRIOR_CONTEXT: &str = r#"You have already sent the user these progress updates earlier in THIS SAME turn (oldest first):
{{prior_notices}}

Do not repeat any of them. Report only what has changed or advanced since the last line. If nothing meaningful has changed yet, say that briefly instead of restating an earlier update.

"#;

/// Per-turn observer state the agent loop threads through each tick: when
/// the last update fired (throttle), the lines already shown (so the next
/// tick can dedupe against the user's real view), and the detached call
/// currently in flight (drained at the next boundary; at most one).
#[derive(Default)]
pub(crate) struct ObserverState {
    pub(crate) last_fired_at: Option<std::time::Instant>,
    pub(crate) sent_notices: Vec<String>,
    /// The spawned observer call, if one is running. Drained (awaited when
    /// finished) at the next iteration boundary before a new one can fire,
    /// enforcing at-most-one-in-flight. Detached on drop — never aborted.
    pub(crate) in_flight: Option<tokio::task::JoinHandle<anyhow::Result<String>>>,
}

/// Build the user turn appended after the cached prefix: the bare
/// instruction on the first tick, or the instruction prefixed with the
/// already-sent lines on later ticks. Pure + cheap, so it's unit-tested
/// without an LLM.
pub(crate) fn build_observer_prompt(prior_notices: &[String]) -> String {
    if prior_notices.is_empty() {
        return PROGRESS_OBSERVER_PROMPT.to_string();
    }
    let joined = prior_notices
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let prefix = PROGRESS_OBSERVER_PRIOR_CONTEXT.replace("{{prior_notices}}", &joined);
    format!("{prefix}{PROGRESS_OBSERVER_PROMPT}")
}

/// Whether `channel` wants the LLM progress narration. True for sidecar
/// channels (telegram / weixin and any out-of-tree sidecar): text-only
/// surfaces with no live work-block view, so a one-line "what's happening
/// now" is the user's only in-turn feedback on a long turn. The web
/// dashboard already streams reasoning + tool steps into its work block —
/// the narration would be redundant there (and previously split that block
/// in two) — and the one-shot CLI / TUI never display it, so both are
/// skipped.
///
/// Sidecars are exactly the gateway's `Multiplexed` channel kinds: anything
/// that isn't the `Subscribed` web (`http`) or CLI/TUI (`tui`). See the
/// `ChannelKind` mapping in the gateway's channel boot.
pub(crate) fn channel_wants_progress(channel: &ChannelType) -> bool {
    *channel != ChannelType::http() && *channel != ChannelType::tui()
}

/// Pure gate (everything except the live cancel check, which the caller
/// does separately). Extracted so the streaming / user / channel /
/// iteration / appear-after / throttle logic is unit-testable without an
/// LLM or real wall-clock waits.
pub(crate) fn should_fire_observer(
    is_streaming: bool,
    trigger_is_user: bool,
    channel_wants_progress: bool,
    iterations: usize,
    turn_started: std::time::Instant,
    last_observer_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    is_streaming
        && trigger_is_user
        && channel_wants_progress
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
    pub(crate) async fn run(
        self,
        request: ChatRequest,
        input_marker: LlmCallInputs,
    ) -> anyhow::Result<String> {
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

        let cancel_ctx = Some((&cancel_token, baybo_job::CancelReason::ParentCancelled));
        // Owned handle so the call below can `select!` on cancellation: an
        // undrainable last summary aborts instead of billing a discarded result.
        let cancel_for_call = cancel_token.clone();
        let begin = LlmCallBegin {
            model_id: model_info.id.clone(),
            provider: model_info.provider.clone(),
            provider_config_hash: String::new(),
            // Marker built at the iteration boundary: the cached transcript
            // prefix referenced by ordinal, the observer prompt inline as
            // the suffix — so each fire's span no longer re-embeds the whole
            // context snapshot.
            input_messages: input_marker,
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
                            reason: baybo_llm::CallReason::ProgressObserver,
                        });
                        let chat = tokio::select! {
                            _ = cancel_for_call.cancelled() => None,
                            r = bound.chat(&request) => Some(r),
                        };
                        let Some(chat) = chat else {
                            // Cancelled before drain: return Err with the token
                            // tripped so the span closes `Cancelled`, not left
                            // `Pending` as an `abort()` would.
                            return (
                                LlmCallResult::default(),
                                Err(anyhow::anyhow!("progress observer cancelled")),
                            );
                        };
                        match chat {
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
    use super::{
        OBSERVER_APPEAR_AFTER, OBSERVER_MIN_INTERVAL, channel_wants_progress, should_fire_observer,
    };
    use baybo_model::ChannelType;
    use std::time::{Duration, Instant};

    #[test]
    fn fires_after_appear_with_no_prior_notice() {
        let t0 = Instant::now();
        assert!(should_fire_observer(
            true,
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
            true,
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
            true,
            5,
            t0,
            None,
            t0 + OBSERVER_APPEAR_AFTER
        ));
    }

    #[test]
    fn never_fires_for_a_channel_that_does_not_want_progress() {
        let t0 = Instant::now();
        // Every timing/trigger gate passes; only the channel is ineligible
        // (the web work block or the CLI), so the observer must stay silent.
        assert!(!should_fire_observer(
            true,
            true,
            false,
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
        assert!(!should_fire_observer(
            true,
            true,
            true,
            9,
            t0,
            Some(now),
            now
        ));
    }

    #[test]
    fn fires_again_once_min_interval_elapses() {
        let t0 = Instant::now();
        let last = t0 + OBSERVER_APPEAR_AFTER;
        let now = last + OBSERVER_MIN_INTERVAL;
        assert!(should_fire_observer(
            true,
            true,
            true,
            9,
            t0,
            Some(last),
            now
        ));
    }

    #[test]
    fn only_sidecar_channels_want_progress() {
        // Sidecars (telegram/weixin) are text-only, so they get the
        // narration; the web has a work block and the CLI/TUI don't show it.
        assert!(channel_wants_progress(&ChannelType::telegram()));
        assert!(channel_wants_progress(&ChannelType::weixin()));
        assert!(!channel_wants_progress(&ChannelType::http()));
        assert!(!channel_wants_progress(&ChannelType::tui()));
    }
}

#[cfg(test)]
mod state_tests {
    use super::ObserverState;

    #[test]
    fn default_has_no_in_flight_call() {
        let state = ObserverState::default();
        assert!(state.in_flight.is_none());
        assert!(state.last_fired_at.is_none());
        assert!(state.sent_notices.is_empty());
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::{PROGRESS_OBSERVER_PROMPT, build_observer_prompt};

    #[test]
    fn empty_history_is_exactly_the_bare_instruction() {
        assert_eq!(build_observer_prompt(&[]), PROGRESS_OBSERVER_PROMPT);
    }

    #[test]
    fn prior_lines_are_listed_before_the_instruction() {
        let prior = vec!["读取了配置".to_string(), "正在跑测试".to_string()];
        let prompt = build_observer_prompt(&prior);

        for line in &prior {
            assert!(prompt.contains(&format!("- {line}")), "missing prior line");
        }
        // The dedup directive and the bare instruction both survive, and the
        // instruction is the trailing suffix (prior context comes first).
        assert!(prompt.contains("Do not repeat any of them."));
        assert!(prompt.ends_with(PROGRESS_OBSERVER_PROMPT));
        let first_prior = prompt.find("- 读取了配置").expect("prior line present");
        let instruction = prompt.find(PROGRESS_OBSERVER_PROMPT).expect("instruction");
        assert!(
            first_prior < instruction,
            "prior lines must precede the ask"
        );
    }
}
