use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use aura_core::{ChatMessage, Role, Session};
use tracing::debug;

use crate::summarize::summary_to_message;
use crate::{CompressResult, ContextManager, ContextSnapshot, SummarizeCallback, Tokenizer};

/// Configuration for the hybrid context manager.
#[derive(Debug, Clone)]
pub struct HybridContextConfig {
    /// Maximum token budget for the entire context window.
    pub max_tokens: usize,
    /// Compression is triggered when current tokens exceed
    /// `max_tokens * compression_threshold`.
    pub compression_threshold: f32,
    /// Number of recent (non-system) messages to always keep outside the
    /// summary window.
    pub keep_recent_messages: usize,
}

impl Default for HybridContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 128_000,
            compression_threshold: 0.8,
            keep_recent_messages: 20,
        }
    }
}

/// Hybrid context manager that combines sliding-window trimming with
/// LLM-based summarization.
///
/// When token usage crosses the threshold, early non-system messages are
/// summarized via the injected `SummarizeCallback` and replaced by a compact
/// summary message. The system prompt and the most recent messages are always
/// preserved.
pub struct HybridContext {
    tokenizer: Arc<dyn Tokenizer>,
    summarizer: Arc<dyn SummarizeCallback>,
    config: HybridContextConfig,
    /// Tracks the compressed summary produced during the session lifetime.
    compressed_summary: std::sync::Mutex<Option<String>>,
}

impl HybridContext {
    pub fn new(
        tokenizer: Arc<dyn Tokenizer>,
        summarizer: Arc<dyn SummarizeCallback>,
        config: HybridContextConfig,
    ) -> Self {
        Self {
            tokenizer,
            summarizer,
            config,
            compressed_summary: std::sync::Mutex::new(None),
        }
    }

    /// Determine the token threshold above which compression fires.
    fn trigger_threshold(&self) -> usize {
        (self.config.max_tokens as f32 * self.config.compression_threshold) as usize
    }

    /// Split messages into (system, non_system) while preserving order within
    /// each group.
    fn partition(messages: &[ChatMessage]) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
        let mut system = Vec::new();
        let mut rest = Vec::new();
        for msg in messages {
            if msg.role == Role::System {
                system.push(msg.clone());
            } else {
                rest.push(msg.clone());
            }
        }
        (system, rest)
    }
}

#[async_trait]
impl ContextManager for HybridContext {
    async fn append(
        &self,
        session: &mut Session,
        role: Role,
        msg: &ChatMessage,
    ) -> aura_core::Result<()> {
        let appended = ChatMessage {
            role,
            content: msg.content.clone(),
        };
        session.messages.push(appended);
        Ok(())
    }

    async fn maybe_compress(&self, session: &mut Session) -> aura_core::Result<CompressResult> {
        let start = Instant::now();
        let before_tokens = self.count_tokens(&session.messages)?;

        if before_tokens <= self.trigger_threshold() {
            return Ok(CompressResult {
                compressed: false,
                before_tokens,
                after_tokens: before_tokens,
                strategy_used: "hybrid".to_string(),
                latency: start.elapsed(),
            });
        }

        debug!(
            before_tokens,
            threshold = self.trigger_threshold(),
            "context compression triggered"
        );

        let (system_msgs, non_system) = Self::partition(&session.messages);

        // If there are not enough non-system messages to split, fall back to
        // a simple sliding-window trim.
        if non_system.len() <= self.config.keep_recent_messages {
            return Ok(CompressResult {
                compressed: false,
                before_tokens,
                after_tokens: before_tokens,
                strategy_used: "hybrid".to_string(),
                latency: start.elapsed(),
            });
        }

        // Split non-system messages into "to summarize" and "to keep".
        let keep_start = non_system
            .len()
            .saturating_sub(self.config.keep_recent_messages);
        let to_summarize = &non_system[..keep_start];
        let to_keep = &non_system[keep_start..];

        // Generate a summary of the older messages.
        let summary_text = self.summarizer.summarize(to_summarize).await?;

        // Validate that the summary is actually shorter than the original.
        let original_tokens: usize = to_summarize
            .iter()
            .map(|m| self.tokenizer.count_message(m))
            .sum();
        let summary_msg = summary_to_message(&summary_text);
        let summary_tokens = self.tokenizer.count_message(&summary_msg);

        // If the summary is not shorter, keep the original messages but still
        // apply a sliding-window fallback so we stay under the limit.
        let mut new_messages = system_msgs;
        if summary_tokens < original_tokens {
            new_messages.push(summary_msg);
            // Store the summary for snapshot purposes.
            if let Ok(mut guard) = self.compressed_summary.lock() {
                *guard = Some(summary_text);
            }
        } else {
            debug!(
                summary_tokens,
                original_tokens, "summary was not shorter; falling back to sliding window"
            );
            // Sliding-window fallback: just keep recent messages.
        }
        new_messages.extend_from_slice(to_keep);

        session.messages = new_messages;
        session.state.compression_count += 1;

        let after_tokens = self.count_tokens(&session.messages)?;

        Ok(CompressResult {
            compressed: true,
            before_tokens,
            after_tokens,
            strategy_used: "hybrid".to_string(),
            latency: start.elapsed(),
        })
    }

    fn count_tokens(&self, messages: &[ChatMessage]) -> aura_core::Result<usize> {
        let total = messages
            .iter()
            .map(|m| self.tokenizer.count_message(m))
            .sum();
        Ok(total)
    }

    fn snapshot(&self, session: &Session) -> ContextSnapshot {
        let token_count = self.count_tokens(&session.messages).unwrap_or(0);
        let summary = self.compressed_summary.lock().ok().and_then(|g| g.clone());
        ContextSnapshot {
            messages: session.messages.clone(),
            compressed_summary: summary,
            token_count,
        }
    }

    fn restore_state(
        &self,
        session: &mut Session,
        snapshot: &ContextSnapshot,
    ) -> aura_core::Result<()> {
        session.messages = snapshot.messages.clone();
        if let Ok(mut guard) = self.compressed_summary.lock() {
            *guard = snapshot.compressed_summary.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::ContentBlock;

    struct SimpleTokenizer;

    impl Tokenizer for SimpleTokenizer {
        fn count_text(&self, text: &str) -> usize {
            // ~1 token per 4 chars
            text.len() / 4 + 1
        }

        fn count_image(&self, _width: u32, _height: u32) -> usize {
            100
        }

        fn count_message(&self, msg: &ChatMessage) -> usize {
            let mut tokens = 4;
            for block in &msg.content {
                match block {
                    ContentBlock::Text(text) => tokens += self.count_text(text),
                    ContentBlock::Image { .. } => tokens += 100,
                    _ => tokens += 50,
                }
            }
            tokens
        }
    }

    struct MockSummarizer;

    #[async_trait]
    impl SummarizeCallback for MockSummarizer {
        async fn summarize(&self, _messages: &[ChatMessage]) -> aura_core::Result<String> {
            Ok("Summary of conversation so far.".to_string())
        }
    }

    fn make_msg(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![ContentBlock::Text(text.to_string())],
        }
    }

    fn make_session(messages: Vec<ChatMessage>) -> Session {
        Session {
            id: "test-session".to_string(),
            user: aura_core::User {
                id: "user-1".to_string(),
                name: None,
                channel: aura_core::ChannelType::Cli,
            },
            channel: aura_core::ChannelType::Cli,
            messages,
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            state: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_no_compression_under_threshold() {
        let tokenizer = Arc::new(SimpleTokenizer);
        let summarizer = Arc::new(MockSummarizer);
        let config = HybridContextConfig {
            max_tokens: 10_000,
            compression_threshold: 0.8,
            keep_recent_messages: 5,
        };
        let ctx = HybridContext::new(tokenizer, summarizer, config);

        let messages = vec![
            make_msg(Role::System, "System prompt"),
            make_msg(Role::User, "Hello"),
            make_msg(Role::Assistant, "Hi"),
        ];
        let mut session = make_session(messages);

        let result = ctx.maybe_compress(&mut session).await.unwrap();
        assert!(!result.compressed);
        assert_eq!(session.messages.len(), 3);
    }

    #[tokio::test]
    async fn test_compression_triggered() {
        let tokenizer = Arc::new(SimpleTokenizer);
        let summarizer = Arc::new(MockSummarizer);
        // Set a very low max_tokens so compression triggers easily.
        let config = HybridContextConfig {
            max_tokens: 50,
            compression_threshold: 0.5,
            keep_recent_messages: 2,
        };
        let ctx = HybridContext::new(tokenizer, summarizer, config);

        let mut messages = vec![make_msg(Role::System, "System prompt")];
        for i in 0..10 {
            messages.push(make_msg(Role::User, &format!("User message number {i}")));
            messages.push(make_msg(
                Role::Assistant,
                &format!("Assistant reply number {i}"),
            ));
        }
        let mut session = make_session(messages);
        let original_len = session.messages.len();

        let result = ctx.maybe_compress(&mut session).await.unwrap();
        assert!(result.compressed);
        assert!(result.after_tokens < result.before_tokens);
        assert!(session.messages.len() < original_len);
        assert_eq!(session.state.compression_count, 1);
    }

    #[tokio::test]
    async fn test_snapshot_preserves_summary() {
        let tokenizer = Arc::new(SimpleTokenizer);
        let summarizer = Arc::new(MockSummarizer);
        let config = HybridContextConfig {
            max_tokens: 50,
            compression_threshold: 0.5,
            keep_recent_messages: 2,
        };
        let ctx = HybridContext::new(tokenizer, summarizer, config);

        let mut messages = vec![make_msg(Role::System, "System prompt")];
        for i in 0..10 {
            messages.push(make_msg(Role::User, &format!("Message {i}")));
            messages.push(make_msg(Role::Assistant, &format!("Reply {i}")));
        }
        let mut session = make_session(messages);

        ctx.maybe_compress(&mut session).await.unwrap();
        let snap = ctx.snapshot(&session);
        assert!(snap.compressed_summary.is_some());

        // Restore into a fresh session
        let mut session2 = make_session(vec![]);
        ctx.restore_state(&mut session2, &snap).unwrap();
        assert_eq!(session2.messages.len(), session.messages.len());
    }
}
