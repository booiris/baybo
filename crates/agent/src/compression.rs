//! Bridge between the agent loop and `aura_context::ContextManager`'s
//! compression callback.
//!
//! `aura-context` deliberately doesn't know about `SpanRecorder`,
//! `CostManager`, or `JobId` — those are agent-layer concerns. The
//! `ContextManager::maybe_compress` API takes an
//! `FnOnce(ChatRequest) -> Fut` so the caller can inject all of that
//! cross-cutting machinery without polluting the context crate.
//!
//! `CompressionRunner` is that injection: it bundles every dependency
//! the compression LLM call needs and exposes a single `run(self, req)`
//! method that the agent loop hands to `maybe_compress` as the chat
//! closure.

use std::sync::Arc;

use aura_llm::GuardedLlm;
use aura_model::JobId;
use aura_trace::{LifecycleOutcome, LlmCallBegin, LlmCallResult, StepKind};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::cost::CostManager;
use crate::security::SecurityGateway;
use crate::trace::SpanRecorder;

/// Agent-side dependencies needed to execute the compression LLM call:
/// trace recorder, cost ledger, the LLM client, and the identity /
/// cancel context that pin the call to a specific job + session.
pub(crate) struct CompressionRunner {
    pub(crate) llm_client: Arc<GuardedLlm>,
    pub(crate) recorder: Arc<SpanRecorder>,
    pub(crate) cost_manager: Option<Arc<CostManager>>,
    /// Same gateway as the main LLM path. Compression LLM output and
    /// errors are scrubbed through it before they land in the trace,
    /// the cost record's joinable content, or the [Conversation
    /// Summary] re-injected into the session — otherwise a model
    /// tricked by prompt injection in the messages it's summarizing
    /// could leak secret-like text into persisted state.
    pub(crate) security_gateway: Arc<SecurityGateway>,
    pub(crate) job_id: JobId,
    pub(crate) user_id: String,
    pub(crate) session_id: aura_model::SessionId,
    pub(crate) model_info: aura_llm::ModelInfo,
    pub(crate) cancel_token: CancellationToken,
}

impl CompressionRunner {
    /// Execute the compression LLM call. Brackets it in a
    /// `StepKind::Compression` step + `LlmCall` span (real lifecycle —
    /// not a post-hoc placeholder), records the cost row against the
    /// span_id while the span is open, and returns the sanitized
    /// `LlmResponse`. `maybe_compress` then trims the content and
    /// applies the strategy's `on_failure` slice if the response is
    /// empty or this method returns `Err`.
    pub(crate) async fn run(
        self,
        request: aura_llm::ChatRequest,
    ) -> Result<aura_llm::LlmResponse, aura_context::ContextError> {
        let CompressionRunner {
            llm_client,
            recorder,
            cost_manager,
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
            // Compression LLM input is a one-off slice the caller
            // built off the active transcript — it never lands in
            // `session_messages`, so embed inline rather than
            // referencing an ordinal that doesn't exist.
            input_messages: aura_trace::LlmCallInputs::Inline(request.messages.clone()),
            temperature: request.temperature,
        };

        let recorder_inner = Arc::clone(&recorder);
        crate::scope::with_step(
            recorder.as_ref(),
            job_id,
            StepKind::Compression,
            cancel_ctx,
            |step| async move {
                let response = crate::scope::with_llm_span(
                    recorder_inner.as_ref(),
                    &step,
                    job_id,
                    begin,
                    cancel_ctx,
                    |span| async move {
                        match llm_client.chat(&request).await {
                            Ok(mut response) => {
                                if let Err(e) =
                                    security_gateway.sanitize_llm_response(&mut response).await
                                {
                                    warn!(
                                        error = %e,
                                        "failed to sanitize compression LLM response"
                                    );
                                }
                                if let Some(cm) = &cost_manager {
                                    cm.record_call(
                                        &user_id,
                                        session_id.clone(),
                                        job_id,
                                        span.span_id,
                                        &model_info.id,
                                        response.usage.input_tokens,
                                        response.usage.output_tokens,
                                        response.usage.cached_input_tokens,
                                        response.usage.cache_creation_input_tokens,
                                    );
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
                                (call_result, Ok(response))
                            }
                            Err(e) => {
                                let raw = e.to_string();
                                let error_msg =
                                    security_gateway.sanitize_error(&raw).await.unwrap_or(raw);
                                let call_result = LlmCallResult {
                                    output_content: String::new(),
                                    thinking: None,
                                    tool_calls: vec![],
                                    input_tokens: 0,
                                    output_tokens: 0,
                                    cached_input_tokens: 0,
                                    cache_creation_input_tokens: 0,
                                };
                                (call_result, Err(anyhow::anyhow!(error_msg)))
                            }
                        }
                    },
                )
                .await?;
                Ok((LifecycleOutcome::Ok, response))
            },
        )
        .await
        .map_err(|e| aura_context::ContextError::Compression(e.to_string()))
    }
}
