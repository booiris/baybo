//! Conversation-title LLM call.

use std::sync::Arc;

use baybo_context::prompts::title::{build_title_prompt, sanitize_title};
use baybo_llm::{Attribution, BillableLlm, ChatRequest, ModelInfo};
use baybo_model::{ChatMessage, ContentBlock, JobId, SessionId};
use baybo_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallInputs, LlmCallResult, SpanRecorder, StepKind,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::security::SecurityGateway;

/// Notified after a title is persisted so a display surface can push it live.
pub trait SessionTitleSink: Send + Sync {
    fn title_updated(&self, session_id: &SessionId, title: &str);
}

pub(crate) struct TitleRunner {
    pub(crate) llm_client: Arc<BillableLlm>,
    pub(crate) recorder: Arc<SpanRecorder>,
    pub(crate) security_gateway: Arc<SecurityGateway>,
    pub(crate) job_id: JobId,
    pub(crate) user_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) model_info: ModelInfo,
    pub(crate) cancel_token: CancellationToken,
}

impl TitleRunner {
    pub(crate) async fn run(self, question: String) -> anyhow::Result<Option<String>> {
        let TitleRunner {
            llm_client,
            recorder,
            security_gateway,
            job_id,
            user_id,
            session_id,
            model_info,
            cancel_token,
        } = self;

        let messages = vec![ChatMessage::user(vec![ContentBlock::Text(
            build_title_prompt(&question),
        )])];
        let request = ChatRequest {
            messages: messages.clone(),
            temperature: None,
            tools: Vec::new(),
            reasoning_effort: None,
        };

        let cancel_ctx = Some((&cancel_token, baybo_job::CancelReason::ParentCancelled));
        let begin = LlmCallBegin {
            model_id: model_info.id.clone(),
            provider: model_info.provider.clone(),
            provider_config_hash: String::new(),
            input_messages: LlmCallInputs::Inline(messages),
            temperature: request.temperature,
        };

        let recorder_inner = Arc::clone(&recorder);
        crate::runtime::scope::with_step(
            recorder.as_ref(),
            job_id,
            StepKind::TitleGeneration,
            cancel_ctx,
            |step| async move {
                let title = crate::runtime::scope::with_llm_span(
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
                            reason: baybo_llm::CallReason::Title,
                        });
                        match bound.chat(&request).await {
                            Ok(billed) => {
                                let mut response = billed.response;
                                if let Err(e) =
                                    security_gateway.sanitize_llm_response(&mut response).await
                                {
                                    warn!(error = %e, "title: sanitize_llm_response failed");
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
                                (call_result, Ok(sanitize_title(&response.content)))
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
                Ok((LifecycleOutcome::Ok, title))
            },
        )
        .await
    }
}
