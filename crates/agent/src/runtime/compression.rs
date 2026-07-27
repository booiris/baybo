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

use baybo_llm::{Attribution, BillableLlm, LlmResponse, ModelInfo};
use baybo_model::{JobId, SessionId};
use baybo_trace::{
    CompressionApplied, CompressionTrigger, LifecycleOutcome, LlmCallBegin, LlmCallResult,
    SpanRecorder, StepKind,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::security::SecurityGateway;

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
    /// trace can tell a send-time compaction — which the turn blocks on and
    /// which reshapes the next prompt — from a detached background pass.
    pub(crate) trigger: CompressionTrigger,
}

impl CompressionRunner {
    /// Execute the compaction LLM call. Brackets it in a
    /// `StepKind::Compression` step + `LlmCall` span (real lifecycle —
    /// not a post-hoc placeholder), records the cost row against the
    /// span_id while the span is open, and returns the sanitized
    /// `LlmResponse`.
    ///
    /// The compressor then parses the content, and applies its truncate
    /// fallback if the response is empty or this method returns `Err`.
    pub(crate) async fn run(
        self,
        request: baybo_llm::ChatRequest,
        input_marker: baybo_trace::LlmCallInputs,
    ) -> Result<LlmResponse, baybo_context::ContextError> {
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

        let cancel_ctx = Some((&cancel_token, baybo_job::CancelReason::ParentCancelled));
        let begin = LlmCallBegin {
            model_id: model_info.id.clone(),
            provider: model_info.provider.clone(),
            provider_config_hash: String::new(),
            // Marker supplied by the caller: a `Persisted` ordinal
            // reference to the transcript prefix (so the span doesn't
            // re-embed the whole summarized window) plus an inline
            // suffix for the framing/sub-loop turns that aren't in
            // `session_messages`. The compressor owns the split because
            // it's the layer that knows which slice is persisted.
            input_messages: input_marker,
            temperature: request.temperature,
        };

        let recorder_inner = Arc::clone(&recorder);
        crate::runtime::scope::with_step(
            recorder.as_ref(),
            job_id,
            StepKind::Compression {
                trigger: Some(trigger),
                // The callback is only reached by the live-summary stage; the
                // truncate fallback never calls it.
                applied: Some(CompressionApplied::LiveSummary),
            },
            cancel_ctx,
            |step| async move {
                let result = crate::runtime::scope::with_llm_span(
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
                            reason: baybo_llm::CallReason::Compression,
                        });
                        match bound.chat(&request).await {
                            Ok(billed) => {
                                let mut response = billed.response;
                                // Scrub before the summary lands in the
                                // trace, the cost row's joinable content,
                                // or the re-injected [Conversation
                                // Summary]. Placeholders are kept, not
                                // revealed — the summarized brain operates
                                // against the sanitized transcript.
                                if let Err(e) =
                                    security_gateway.sanitize_llm_response(&mut response).await
                                {
                                    warn!(error = %e, "compression: sanitize_llm_response failed");
                                }
                                let call_result = LlmCallResult {
                                    output_content: response.content.clone(),
                                    thinking: response.thinking.clone(),
                                    // The compaction request offers no tools
                                    // (`ChatRequest.tools` is empty), so there
                                    // is never anything to record here.
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
                                let raw = e.to_string();
                                let msg =
                                    security_gateway.sanitize_error(&raw).await.unwrap_or(raw);
                                (LlmCallResult::default(), Err(anyhow::anyhow!(msg)))
                            }
                        }
                    },
                )
                .await?;
                Ok((LifecycleOutcome::Ok, result))
            },
        )
        .await
        .map_err(|e| baybo_context::ContextError::Compression(e.to_string()))
    }
}
