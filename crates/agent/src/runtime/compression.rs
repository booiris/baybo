//! Agent-side wiring for the compaction LLM call.
//!
//! `baybo-context` deliberately doesn't know about `SpanRecorder`,
//! `CostManager`, or `JobId` — those are agent-layer concerns, so
//! `ContextManager::maybe_compress` takes a chat callback and the caller
//! injects all of that cross-cutting machinery without polluting the
//! context crate. [`CompressionRunner`] is that injection: it bundles
//! every dependency the call needs and exposes a single `run(self, req)`
//! the agent loop hands to `maybe_compress` (threshold) and
//! `force_compress` (`/compact`).
//!
//! See `docs/modules/context.md`.

use std::sync::Arc;

use baybo_context::ContextError;
use baybo_llm::{Attribution, BillableLlm, LlmResponse, ModelInfo};
use baybo_model::{JobId, SessionId};
use baybo_trace::{
    CompressionApplied, CompressionTrigger, LifecycleOutcome, LlmCallBegin, LlmCallResult,
    SpanRecorder, StepKind,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::security::SecurityGateway;

/// How many summarizer attempts one compaction gets. The second exists
/// because the alternative to a summary is the truncate fallback, which
/// throws away the middle of the conversation for good — worth one more
/// round-trip before paying that. Bounded at two: the request is the whole
/// transcript, so each attempt is the most expensive call the session makes.
const MAX_COMPACTION_ATTEMPTS: usize = 2;

/// Agent-side dependencies needed to execute the compression LLM call:
/// trace recorder, cost ledger, the LLM client, and the identity /
/// cancel context that pin the call to a specific job + session.
pub(crate) struct CompressionRunner {
    pub(crate) llm_client: Arc<BillableLlm>,
    pub(crate) recorder: Arc<SpanRecorder>,
    /// Same gateway as the main LLM path. Compression LLM output and
    /// errors are scrubbed through it before they land in the trace,
    /// the cost record's joinable content, or the [Conversation
    /// Summary] re-injected into the session — otherwise a model
    /// tricked by prompt injection in the messages it's summarizing
    /// could leak secret-like text into persisted state.
    pub(crate) security_gateway: Arc<SecurityGateway>,
    pub(crate) job_id: JobId,
    pub(crate) user_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) model_info: ModelInfo,
    pub(crate) cancel_token: CancellationToken,
    /// Which path is running this compaction. Recorded on the step so a
    /// trace can tell a threshold compaction from a user-typed `/compact`.
    pub(crate) trigger: CompressionTrigger,
}

impl CompressionRunner {
    /// Execute the compaction LLM call. Brackets it in a
    /// `StepKind::Compression` step (one per compaction) around up to
    /// [`MAX_COMPACTION_ATTEMPTS`] `LlmCall` spans, records a cost row
    /// against each span while it is open, and returns the sanitized
    /// `LlmResponse`.
    ///
    /// The compressor parses the content and applies its truncate fallback
    /// if the response is empty or this returns [`ContextError::Compression`]
    /// — but **not** on [`ContextError::Cancelled`], which leaves the
    /// transcript alone.
    pub(crate) async fn run(
        self,
        request: baybo_llm::ChatRequest,
        input_marker: baybo_trace::LlmCallInputs,
    ) -> Result<LlmResponse, ContextError> {
        let CompressionRunner {
            llm_client,
            recorder,
            security_gateway,
            job_id,
            user_id,
            session_id,
            model_info,
            cancel_token,
            trigger,
        } = self;

        // Borrowed for span/step outcome classification only. The compaction
        // call is deliberately NOT raced against it — see below.
        let cancel_ctx = Some((&cancel_token, baybo_job::CancelReason::ParentCancelled));
        let recorder_inner = Arc::clone(&recorder);
        crate::runtime::scope::with_step(
            recorder.as_ref(),
            job_id,
            StepKind::Compression {
                trigger: Some(trigger),
                // The callback is only reached by the live-summary path; the
                // truncate fallback never calls it.
                applied: Some(CompressionApplied::LiveSummary),
            },
            cancel_ctx,
            |step| async move {
                let mut last_error: Option<String> = None;
                for attempt in 0..MAX_COMPACTION_ATTEMPTS {
                    let begin = LlmCallBegin {
                        model_id: model_info.id.clone(),
                        provider: model_info.provider.clone(),
                        provider_config_hash: String::new(),
                        // A `Persisted` ordinal reference to the transcript
                        // prefix (so the span doesn't re-embed the whole
                        // summarized window) plus an inline suffix for the
                        // framing turns that aren't in `session_messages`.
                        // The compressor owns the split because it's the
                        // layer that knows which slice is persisted.
                        input_messages: input_marker.clone(),
                        temperature: request.temperature,
                    };
                    // Whether the failure this attempt saw is worth a second
                    // call. Decided on the TYPED error inside the span body,
                    // where it still exists — one layer up it is a string.
                    let retriable = Arc::new(parking_lot::Mutex::new(false));
                    let result = crate::runtime::scope::with_llm_span(
                        recorder_inner.as_ref(),
                        &step,
                        job_id,
                        begin,
                        cancel_ctx,
                        |span| {
                            let llm_client = Arc::clone(&llm_client);
                            let security_gateway = Arc::clone(&security_gateway);
                            let retriable = Arc::clone(&retriable);
                            let user_id = user_id.clone();
                            let session_id = session_id.clone();
                            let request = &request;
                            async move {
                                let bound = llm_client.bind(Attribution {
                                    user_id,
                                    session_id,
                                    job_id,
                                    span_id: span.span_id,
                                    reason: baybo_llm::CallReason::Compression,
                                });
                                // NOT raced against the cancel token, on
                                // purpose. `/stop` means "stop working on my
                                // request", and a compaction is housekeeping
                                // that makes the NEXT turn cheaper — the
                                // request is already in flight and already
                                // being billed, so abandoning it throws that
                                // spend away and leaves the transcript
                                // over-budget for the next turn to compact
                                // from scratch. Letting it land means the
                                // money already spent buys something.
                                match bound.chat(request).await {
                                    Ok(billed) => {
                                        let mut response = billed.response;
                                        // Scrub before the summary lands in
                                        // the trace, the cost row's joinable
                                        // content, or the re-injected
                                        // [Conversation Summary]. Placeholders
                                        // are kept, not revealed — the
                                        // summarized brain operates against
                                        // the sanitized transcript.
                                        if let Err(e) = security_gateway
                                            .sanitize_llm_response(&mut response)
                                            .await
                                        {
                                            warn!(
                                                error = %e,
                                                "compaction: sanitize_llm_response failed"
                                            );
                                        }
                                        let call_result = LlmCallResult {
                                            output_content: response.content.clone(),
                                            thinking: response.thinking.clone(),
                                            // The compaction request offers no
                                            // tools, so there is never
                                            // anything to record here.
                                            tool_calls: Vec::new(),
                                            input_tokens: response.usage.input_tokens,
                                            output_tokens: response.usage.output_tokens,
                                            cached_input_tokens: response.usage.cached_input_tokens,
                                            cache_creation_input_tokens: response
                                                .usage
                                                .cache_creation_input_tokens,
                                        };
                                        (call_result, Ok(response))
                                    }
                                    Err(e) => {
                                        // Classify BEFORE flattening to a
                                        // string: a context-window 400 is the
                                        // likeliest compaction failure, and
                                        // retrying it just buys the same 400.
                                        *retriable.lock() = e.is_retriable();
                                        let raw = e.to_string();
                                        let msg = security_gateway
                                            .sanitize_error(&raw)
                                            .await
                                            .unwrap_or(raw);
                                        (LlmCallResult::default(), Err(anyhow::anyhow!(msg)))
                                    }
                                }
                            }
                        },
                    )
                    .await;

                    let error = match result {
                        Ok(response) => return Ok((LifecycleOutcome::Ok, Ok(response))),
                        Err(e) => e.to_string(),
                    };
                    let retriable = *retriable.lock();
                    if !retriable || attempt + 1 == MAX_COMPACTION_ATTEMPTS {
                        return Ok((LifecycleOutcome::Ok, Err(ContextError::Compression(error))));
                    }
                    debug!(
                        attempt = attempt + 1,
                        max = MAX_COMPACTION_ATTEMPTS,
                        %error,
                        "compaction call failed; retrying before the truncate fallback"
                    );
                    last_error = Some(error);
                }
                Ok((
                    LifecycleOutcome::Ok,
                    Err(ContextError::Compression(
                        last_error.unwrap_or_else(|| "compaction failed".to_string()),
                    )),
                ))
            },
        )
        .await
        .unwrap_or_else(|e| Err(ContextError::Compression(e.to_string())))
    }
}
