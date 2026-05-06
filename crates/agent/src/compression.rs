//! In-loop context compression via LLM summarization.
//!
//! `LlmSummarizer` is the production [`SummarizeCallback`] implementation
//! injected into [`aura_context::Summarize`] at runtime. It wraps the
//! main agent's [`LlmCompletion`] (no separate model — see Q2 of the
//! original design grilling) so summarization spend is recorded against
//! the same provider and lands in the same cost ledger as the main
//! turns.
//!
//! Failure handling is deliberately one-sided: any error returned from
//! here propagates up to `Summarize::compress`, which catches it and
//! falls back to a Truncate-equivalent slice. We never retry, never
//! degrade to a different model, never paper over the failure here —
//! the strategy layer owns the fallback policy.

use std::sync::Arc;

use async_trait::async_trait;
use aura_context::{CompressionLlmCall, ContextError, SummarizeCallback, SummarizeOutput};
use aura_llm::{ChatRequest, LlmCompletion};
use aura_model::{ChatMessage, ContentBlock, Role};

const SUMMARIZE_INSTRUCTION: &str = "\
You are summarizing the older portion of an agent's own conversation \
so it can continue the same task. Preserve: the user's current request \
and any constraints; recent tool calls and the key facts they returned \
(file paths, IDs, error messages); decisions already made; open todos; \
anything the agent must remember to finish the task. Drop: redundant \
exchanges, exploratory dead-ends. Output plain prose, no preamble.";

/// LLM-backed implementation of [`SummarizeCallback`] used by the
/// [`aura_context::Summarize`] strategy in the agent runtime.
pub struct LlmSummarizer {
    client: Arc<dyn LlmCompletion>,
}

impl LlmSummarizer {
    pub fn new(client: Arc<dyn LlmCompletion>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl SummarizeCallback for LlmSummarizer {
    async fn summarize(&self, messages: &[ChatMessage]) -> aura_context::Result<SummarizeOutput> {
        let mut request_messages: Vec<ChatMessage> = messages.to_vec();
        request_messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(SUMMARIZE_INSTRUCTION.to_string())],
        });

        let request = ChatRequest {
            messages: request_messages,
            temperature: None,
            tools: Vec::new(),
        };

        let response = self
            .client
            .chat(&request)
            .await
            .map_err(|e| ContextError::Compression(e.to_string()))?;

        let summary = response.content.trim().to_string();
        if summary.is_empty() {
            return Err(ContextError::EmptySummary);
        }

        let info = self.client.model_info();
        let llm_call = CompressionLlmCall {
            model_id: info.id.clone(),
            provider: info.provider.clone(),
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cached_input_tokens: response.usage.cached_input_tokens,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
        };

        Ok(SummarizeOutput { summary, llm_call })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_llm::{
        BlobFetcher, LlmError, LlmResponse, LlmStream, ModelInfo, ModelPricing, TokenUsage,
    };
    use aura_model::MicroUsd;
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn make_info() -> ModelInfo {
        ModelInfo {
            id: "test-model".into(),
            provider: "test-provider".into(),
            context_window: 100_000,
            supports_tools: false,
            supports_vision: false,
            pricing: ModelPricing {
                input_per_1m_tokens: MicroUsd::ZERO,
                output_per_1m_tokens: MicroUsd::ZERO,
            },
        }
    }

    fn make_response(content: &str) -> LlmResponse {
        LlmResponse {
            content: content.to_string(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: TokenUsage {
                input_tokens: 1000,
                output_tokens: 200,
                cached_input_tokens: 50,
                cache_creation_input_tokens: 25,
            },
            thinking: None,
        }
    }

    /// Fake `LlmCompletion`: returns a single canned response or error,
    /// captures the request that was sent.
    struct FakeLlm {
        info: ModelInfo,
        response: Mutex<Option<aura_llm::Result<LlmResponse>>>,
        captured: Mutex<Option<ChatRequest>>,
    }

    impl FakeLlm {
        fn new(response: aura_llm::Result<LlmResponse>) -> Self {
            Self {
                info: make_info(),
                response: Mutex::new(Some(response)),
                captured: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmCompletion for FakeLlm {
        async fn chat(&self, request: &ChatRequest) -> aura_llm::Result<LlmResponse> {
            *self.captured.lock() = Some(request.clone());
            self.response
                .lock()
                .take()
                .expect("FakeLlm: chat called more than once")
        }

        async fn chat_stream(&self, _request: &ChatRequest) -> aura_llm::Result<LlmStream> {
            unimplemented!("FakeLlm does not support streaming")
        }

        fn model_info(&self) -> &ModelInfo {
            &self.info
        }
    }

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    #[tokio::test]
    async fn success_populates_summary_and_compression_llm_call() {
        let fake = Arc::new(FakeLlm::new(Ok(make_response("  condensed summary  "))));
        let summarizer = LlmSummarizer::new(fake.clone());

        let input = vec![user_msg("a"), user_msg("b")];
        let out = summarizer.summarize(&input).await.unwrap();

        assert_eq!(out.summary, "condensed summary");
        assert_eq!(out.llm_call.model_id, "test-model");
        assert_eq!(out.llm_call.provider, "test-provider");
        assert_eq!(out.llm_call.input_tokens, 1000);
        assert_eq!(out.llm_call.output_tokens, 200);
        assert_eq!(out.llm_call.cached_input_tokens, 50);
        assert_eq!(out.llm_call.cache_creation_input_tokens, 25);

        let captured = fake.captured.lock().clone().expect("chat was called");
        assert_eq!(captured.messages.len(), input.len() + 1);
        assert!(captured.tools.is_empty());
        assert!(captured.temperature.is_none());

        let trailing = captured.messages.last().unwrap();
        assert_eq!(trailing.role, Role::User);
        match &trailing.content[0] {
            ContentBlock::Text(t) => {
                assert!(t.contains("summarizing the older portion"));
                assert!(t.contains("Output plain prose, no preamble."));
            }
            _ => panic!("expected text trailing instruction"),
        }
    }

    #[tokio::test]
    async fn empty_response_returns_empty_summary_error() {
        let fake = Arc::new(FakeLlm::new(Ok(make_response("   \n  "))));
        let summarizer = LlmSummarizer::new(fake);
        let err = summarizer.summarize(&[user_msg("x")]).await.unwrap_err();
        assert!(matches!(err, ContextError::EmptySummary), "got: {err:?}");
    }

    #[tokio::test]
    async fn chat_error_propagates_as_compression_error() {
        let fake = Arc::new(FakeLlm::new(Err(LlmError::Provider("rate-limited".into()))));
        let summarizer = LlmSummarizer::new(fake);
        let err = summarizer.summarize(&[user_msg("x")]).await.unwrap_err();
        match err {
            ContextError::Compression(msg) => {
                assert!(msg.contains("rate-limited"), "got: {msg}");
            }
            other => panic!("expected Compression error, got: {other:?}"),
        }
    }

    // Compile-time check: BlobFetcher trait stays importable from this
    // crate without dragging in extra symbols. Removable once another
    // call site uses it.
    #[allow(dead_code)]
    fn _blob_fetcher_marker(_: &dyn BlobFetcher) {}
}
