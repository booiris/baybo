pub mod hybrid;
pub mod sliding_window;
pub mod summarize;

use std::time::Duration;

use async_trait::async_trait;
use aura_core::{ChatMessage, Role, Session};
use serde::{Deserialize, Serialize};

/// Trait for counting tokens in text and multimodal content.
///
/// Defined in this crate but implemented externally (e.g. by the `llm` crate)
/// so that `context` remains independent of any specific LLM provider.
pub trait Tokenizer: Send + Sync {
    /// Count the number of tokens in a text string.
    fn count_text(&self, text: &str) -> usize;

    /// Count the token cost of an image given its dimensions.
    fn count_image(&self, width: u32, height: u32) -> usize;

    /// Count the total tokens in a chat message, including structural overhead
    /// such as role markers and separators.
    fn count_message(&self, msg: &ChatMessage) -> usize;
}

/// Callback for generating summaries of message batches.
///
/// This keeps the `context` crate independent from `llm` -- the agent layer
/// injects a concrete implementation that calls the LLM.
#[async_trait]
pub trait SummarizeCallback: Send + Sync {
    /// Summarize the given messages into a single condensed text string.
    async fn summarize(&self, messages: &[ChatMessage]) -> aura_core::Result<String>;
}

/// Manages session context: appending messages, compression, snapshots.
#[async_trait]
pub trait ContextManager: Send + Sync {
    /// Append a message with the given role to the session context.
    async fn append(
        &self,
        session: &mut Session,
        role: Role,
        msg: &ChatMessage,
    ) -> aura_core::Result<()>;

    /// Check whether compression is needed and perform it if so.
    async fn maybe_compress(&self, session: &mut Session) -> aura_core::Result<CompressResult>;

    /// Count the total tokens across a slice of messages.
    fn count_tokens(&self, messages: &[ChatMessage]) -> aura_core::Result<usize>;

    /// Create a snapshot of the current session context.
    fn snapshot(&self, session: &Session) -> ContextSnapshot;

    /// Restore the session context from a previously captured snapshot.
    fn restore_state(
        &self,
        session: &mut Session,
        snapshot: &ContextSnapshot,
    ) -> aura_core::Result<()>;
}

/// Result of a compression attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressResult {
    /// Whether compression actually occurred.
    pub compressed: bool,
    /// Token count before compression.
    pub before_tokens: usize,
    /// Token count after compression.
    pub after_tokens: usize,
    /// Name of the strategy that was used (e.g. "sliding_window", "hybrid").
    pub strategy_used: String,
    /// Wall-clock time spent on compression.
    pub latency: Duration,
}

/// A point-in-time snapshot of the session context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    /// The full message history at the time of the snapshot.
    pub messages: Vec<ChatMessage>,
    /// An optional compressed summary that replaced earlier messages.
    pub compressed_summary: Option<String>,
    /// Total token count at the time of the snapshot.
    pub token_count: usize,
}
