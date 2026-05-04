pub mod summarize;
pub mod truncate;

use async_trait::async_trait;
use aura_model::ChatMessage;

use crate::tokenizer::Tokenizer;

/// Strategy for compressing context messages when the token budget is exceeded.
///
/// Implementations receive the full message list and a tokenizer, and return
/// a (possibly shorter) message list. The `ContextManager` handles the
/// budget tracking and compression triggering; the strategy only decides
/// *how* to compress.
#[async_trait]
pub trait CompressionStrategy: Send + Sync {
    /// Compress the given messages, returning a reduced message list.
    async fn compress(
        &self,
        messages: &[ChatMessage],
        tokenizer: &dyn Tokenizer,
    ) -> crate::Result<CompressOutput>;
}

/// Output of a compression operation.
pub struct CompressOutput {
    /// The compressed message list.
    pub messages: Vec<ChatMessage>,
    /// Set when the strategy made an LLM call. Propagated upward by
    /// `ContextManager` so the agent loop can record the call as a
    /// `SpanKind::LlmCall` after the fact (see `CompressStats::llm_call`).
    pub llm_call: Option<crate::CompressionLlmCall>,
}

/// Output of [`SummarizeCallback::summarize`].
///
/// Carries both the summary text and the provenance/usage of the
/// underlying LLM call so the strategy can surface it via
/// `CompressOutput::llm_call`.
pub struct SummarizeOutput {
    pub summary: String,
    pub llm_call: crate::CompressionLlmCall,
}

/// Callback for LLM-based summarization of context messages.
///
/// Defined in this crate but implemented externally to keep `context`
/// independent from `llm`. Injected into `Summarize` strategy at construction.
#[async_trait]
pub trait SummarizeCallback: Send + Sync {
    /// Summarize a sequence of messages, returning both the text and
    /// the LLM call's provenance/usage.
    async fn summarize(&self, messages: &[ChatMessage]) -> crate::Result<SummarizeOutput>;
}
