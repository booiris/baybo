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
}

/// Callback for LLM-based summarization of context messages.
///
/// Defined in this crate but implemented externally to keep `context`
/// independent from `llm`. Injected into `Summarize` strategy at construction.
#[async_trait]
pub trait SummarizeCallback: Send + Sync {
    /// Summarize a sequence of messages into a shorter text representation.
    async fn summarize(&self, messages: &[ChatMessage]) -> crate::Result<String>;
}
