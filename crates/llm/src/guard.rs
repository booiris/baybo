//! Pre-call guard wrapper for [`LlmCompletion`].
//!
//! [`GuardedLlm`] runs an injected closure before each
//! `chat` / `chat_stream` call and short-circuits with
//! [`LlmError::GuardRejected`] if the closure says no. The closure
//! captures whatever caller-side state it needs (cost budgets, rate
//! limits, kill switches) so this crate never grows knowledge of
//! sessions, jobs, or spans.
//!
//! Wrap the raw client once at startup and hand the resulting
//! `Arc<GuardedLlm>` to every consumer (the agent loop *and* any
//! in-process summarizer / retriever / probe). The handle is sealed
//! — `GuardedLlm` deliberately does *not* implement `LlmCompletion`,
//! so `Arc<GuardedLlm>` cannot be unsizing-coerced back to
//! `Arc<dyn LlmCompletion>`. The only public way to mint one is
//! [`GuardedLlm::new`], which requires a guard. Production
//! constructors (`AgentLoop`, `ToolRegistry`, etc.) take
//! `Arc<GuardedLlm>` directly so the type alone refuses a raw client.

use std::sync::Arc;

use crate::{ChatRequest, LlmCompletion, LlmError, LlmResponse, LlmStream, ModelInfo};

/// Closure invoked before every guarded LLM call. Returns `Err` to
/// reject the call before the provider is contacted.
pub type LlmCallGuard = Arc<dyn Fn() -> Result<(), LlmError> + Send + Sync>;

/// Sealed LLM client handle. Holding one is type-level proof that an
/// [`LlmCallGuard`] runs before every `chat` / `chat_stream`.
pub struct GuardedLlm {
    inner: Arc<dyn LlmCompletion>,
    guard: LlmCallGuard,
}

impl GuardedLlm {
    /// Wrap `inner` with `guard` and return the sealed handle.
    /// Returns `Arc<Self>` directly because every realistic consumer
    /// wants cheap clones, and constructing the `Arc` here keeps the
    /// caller from accidentally building a non-shared `GuardedLlm`
    /// they then can't fan out.
    pub fn new(inner: Arc<dyn LlmCompletion>, guard: LlmCallGuard) -> Arc<Self> {
        Arc::new(Self { inner, guard })
    }

    /// Test-only construction with a pass-through guard that always
    /// admits the call. Use sparingly — every call site is a place
    /// where the gate intentionally doesn't fire (CLI probes, gateway
    /// fixtures, unit tests). Gated behind `cfg(any(test,
    /// feature = "test-support"))` so a release build can never reach
    /// it accidentally.
    #[cfg(any(test, feature = "test-support"))]
    pub fn passthrough(inner: Arc<dyn LlmCompletion>) -> Arc<Self> {
        Self::new(inner, Arc::new(|| Ok(())))
    }

    pub async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse> {
        (self.guard)()?;
        self.inner.chat(request).await
    }

    pub async fn chat_stream(&self, request: &ChatRequest) -> crate::Result<LlmStream> {
        (self.guard)()?;
        self.inner.chat_stream(request).await
    }

    pub fn model_info(&self) -> &ModelInfo {
        self.inner.model_info()
    }

    /// Issue a minimal chat request to verify provider connectivity
    /// and auth. Mirrors the previous `LlmClient::probe()` so the
    /// `aura llm probe` / `aura doctor` paths still have a cheap
    /// one-token check, but routed through the gate so the same
    /// budget / kill-switch policies apply.
    pub async fn probe(&self) -> crate::Result<crate::ProbeReport> {
        let req = ChatRequest {
            messages: vec![aura_model::ChatMessage {
                role: aura_model::Role::User,
                content: vec![aura_model::ContentBlock::Text("ping".to_string())],
                from_user: false,
            }],
            temperature: Some(0.0),
            tools: vec![],
        };
        let start = std::time::Instant::now();
        let response = self.chat(&req).await?;
        let info = self.inner.model_info();
        Ok(crate::ProbeReport {
            provider: info.provider.clone(),
            model: info.id.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
            tokens: response.usage,
            thinking_chars: response.thinking.as_ref().map(|s| s.chars().count()),
            thinking_preview: response
                .thinking
                .as_ref()
                .map(|s| s.lines().next().unwrap_or("").chars().take(120).collect()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelPricing, TokenUsage};
    use async_trait::async_trait;
    use parking_lot::Mutex;

    fn make_info() -> ModelInfo {
        ModelInfo {
            id: "test-model".into(),
            provider: "test-provider".into(),
            context_window: 100_000,
            supports_tools: false,
            supports_vision: false,
            pricing: ModelPricing::default(),
        }
    }

    fn make_response() -> LlmResponse {
        LlmResponse {
            content: "ok".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: TokenUsage::default(),
            thinking: None,
        }
    }

    /// Counting fake: records how many times each method was invoked
    /// so tests can assert the inner client wasn't called when the
    /// guard rejected.
    struct CountingLlm {
        info: ModelInfo,
        chats: Mutex<u32>,
    }

    impl CountingLlm {
        fn new() -> Self {
            Self {
                info: make_info(),
                chats: Mutex::new(0),
            }
        }
        fn chat_count(&self) -> u32 {
            *self.chats.lock()
        }
    }

    #[async_trait]
    impl LlmCompletion for CountingLlm {
        async fn chat(&self, _: &ChatRequest) -> crate::Result<LlmResponse> {
            *self.chats.lock() += 1;
            Ok(make_response())
        }
        async fn chat_stream(&self, _: &ChatRequest) -> crate::Result<LlmStream> {
            *self.chats.lock() += 1;
            Ok(LlmStream::from_events(vec![]))
        }
        fn model_info(&self) -> &ModelInfo {
            &self.info
        }
    }

    fn empty_request() -> ChatRequest {
        ChatRequest {
            messages: vec![],
            temperature: None,
            tools: vec![],
        }
    }

    #[tokio::test]
    async fn guard_pass_delegates_to_inner() {
        let inner = Arc::new(CountingLlm::new());
        let guard: LlmCallGuard = Arc::new(|| Ok(()));
        let guarded = GuardedLlm::new(inner.clone(), guard);

        guarded.chat(&empty_request()).await.unwrap();
        assert_eq!(inner.chat_count(), 1);
    }

    #[tokio::test]
    async fn guard_reject_short_circuits_chat() {
        let inner = Arc::new(CountingLlm::new());
        let guard: LlmCallGuard = Arc::new(|| Err(LlmError::GuardRejected("over budget".into())));
        let guarded = GuardedLlm::new(inner.clone(), guard);

        let err = guarded.chat(&empty_request()).await.unwrap_err();
        assert!(matches!(err, LlmError::GuardRejected(ref m) if m == "over budget"));
        assert_eq!(inner.chat_count(), 0, "inner client must not be called");
    }

    #[tokio::test]
    async fn guard_reject_short_circuits_chat_stream() {
        let inner = Arc::new(CountingLlm::new());
        let guard: LlmCallGuard = Arc::new(|| Err(LlmError::GuardRejected("over budget".into())));
        let guarded = GuardedLlm::new(inner.clone(), guard);

        match guarded.chat_stream(&empty_request()).await {
            Err(LlmError::GuardRejected(m)) => assert_eq!(m, "over budget"),
            Err(other) => panic!("unexpected error variant: {other:?}"),
            Ok(_) => panic!("expected guard rejection, got Ok"),
        }
        assert_eq!(inner.chat_count(), 0);
    }

    #[tokio::test]
    async fn model_info_passes_through() {
        let inner = Arc::new(CountingLlm::new());
        let guard: LlmCallGuard = Arc::new(|| Ok(()));
        let guarded = GuardedLlm::new(inner.clone(), guard);

        assert_eq!(guarded.model_info().id, "test-model");
        assert_eq!(inner.chat_count(), 0);
    }
}
