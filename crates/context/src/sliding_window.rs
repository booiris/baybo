use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use aura_model::{ChatMessage, Role};
use aura_session::Session;

use crate::{CompressResult, ContextManager, ContextSnapshot, Tokenizer};

/// A simple sliding-window context manager.
///
/// Keeps the system prompt and the most recent `keep_recent` messages,
/// discarding everything in between when the window is exceeded.
pub struct SlidingWindowContext {
    tokenizer: Arc<dyn Tokenizer>,
    keep_recent: usize,
}

impl SlidingWindowContext {
    pub fn new(tokenizer: Arc<dyn Tokenizer>, keep_recent: usize) -> Self {
        Self {
            tokenizer,
            keep_recent,
        }
    }

    /// Partition messages into (system_messages, non_system_messages).
    fn partition_system(messages: &[ChatMessage]) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
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
impl ContextManager for SlidingWindowContext {
    async fn append(
        &self,
        session: &mut Session,
        role: Role,
        msg: &ChatMessage,
    ) -> crate::Result<()> {
        let appended = ChatMessage {
            role,
            content: msg.content.clone(),
        };
        session.messages.push(appended);
        Ok(())
    }

    async fn maybe_compress(&self, session: &mut Session) -> crate::Result<CompressResult> {
        let start = Instant::now();
        let before_tokens = self.count_tokens(&session.messages)?;

        let (system_msgs, non_system) = Self::partition_system(&session.messages);

        if non_system.len() <= self.keep_recent {
            return Ok(CompressResult {
                compressed: false,
                before_tokens,
                after_tokens: before_tokens,
                strategy_used: "sliding_window".to_string(),
                latency: start.elapsed(),
            });
        }

        // Keep only the most recent N non-system messages.
        let kept_start = non_system.len().saturating_sub(self.keep_recent);
        let kept: Vec<ChatMessage> = non_system[kept_start..].to_vec();

        let mut new_messages = system_msgs;
        new_messages.extend(kept);

        session.messages = new_messages;

        let after_tokens = self.count_tokens(&session.messages)?;

        Ok(CompressResult {
            compressed: true,
            before_tokens,
            after_tokens,
            strategy_used: "sliding_window".to_string(),
            latency: start.elapsed(),
        })
    }

    fn count_tokens(&self, messages: &[ChatMessage]) -> crate::Result<usize> {
        let total = messages
            .iter()
            .map(|m| self.tokenizer.count_message(m))
            .sum();
        Ok(total)
    }

    fn snapshot(&self, session: &Session) -> ContextSnapshot {
        let token_count = self.count_tokens(&session.messages).unwrap_or(0);
        ContextSnapshot {
            messages: session.messages.clone(),
            compressed_summary: None,
            token_count,
        }
    }

    fn restore_state(
        &self,
        session: &mut Session,
        snapshot: &ContextSnapshot,
    ) -> crate::Result<()> {
        session.messages = snapshot.messages.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::ContentBlock;

    struct SimpleTokenizer;

    impl Tokenizer for SimpleTokenizer {
        fn count_text(&self, text: &str) -> usize {
            // Rough approximation: 1 token per 4 chars.
            text.len() / 4 + 1
        }

        fn count_image(&self, _width: u32, _height: u32) -> usize {
            100
        }

        fn count_message(&self, msg: &ChatMessage) -> usize {
            let mut tokens = 4; // structural overhead
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

    fn make_msg(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![ContentBlock::Text(text.to_string())],
        }
    }

    fn make_session(messages: Vec<ChatMessage>) -> Session {
        Session {
            id: "test-session".to_string(),
            user: aura_session::User {
                id: "user-1".to_string(),
                name: None,
                channel: aura_session::ChannelType::Cli,
            },
            channel: aura_session::ChannelType::Cli,
            messages,
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            state: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_append_adds_message() {
        let tokenizer = Arc::new(SimpleTokenizer);
        let ctx = SlidingWindowContext::new(tokenizer, 5);
        let mut session = make_session(vec![]);

        let msg = make_msg(Role::User, "hello");
        ctx.append(&mut session, Role::User, &msg).await.unwrap();

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, Role::User);
    }

    #[tokio::test]
    async fn test_compress_keeps_system_and_recent() {
        let tokenizer = Arc::new(SimpleTokenizer);
        let ctx = SlidingWindowContext::new(tokenizer, 2);

        let messages = vec![
            make_msg(Role::System, "You are a helpful assistant."),
            make_msg(Role::User, "Message 1"),
            make_msg(Role::Assistant, "Reply 1"),
            make_msg(Role::User, "Message 2"),
            make_msg(Role::Assistant, "Reply 2"),
            make_msg(Role::User, "Message 3"),
        ];
        let mut session = make_session(messages);

        let result = ctx.maybe_compress(&mut session).await.unwrap();
        assert!(result.compressed);
        assert_eq!(result.strategy_used, "sliding_window");

        // Should have: system prompt + 2 most recent non-system messages
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].role, Role::System);
    }

    #[tokio::test]
    async fn test_no_compress_when_under_limit() {
        let tokenizer = Arc::new(SimpleTokenizer);
        let ctx = SlidingWindowContext::new(tokenizer, 10);

        let messages = vec![
            make_msg(Role::System, "System prompt"),
            make_msg(Role::User, "Hello"),
            make_msg(Role::Assistant, "Hi"),
        ];
        let mut session = make_session(messages);

        let result = ctx.maybe_compress(&mut session).await.unwrap();
        assert!(!result.compressed);
        assert_eq!(result.before_tokens, result.after_tokens);
    }

    #[tokio::test]
    async fn test_snapshot_and_restore() {
        let tokenizer = Arc::new(SimpleTokenizer);
        let ctx = SlidingWindowContext::new(tokenizer, 5);

        let messages = vec![
            make_msg(Role::System, "System prompt"),
            make_msg(Role::User, "Hello"),
        ];
        let mut session = make_session(messages);

        let snap = ctx.snapshot(&session);
        assert_eq!(snap.messages.len(), 2);

        // Mutate the session
        let msg = make_msg(Role::Assistant, "Hi there");
        ctx.append(&mut session, Role::Assistant, &msg)
            .await
            .unwrap();
        assert_eq!(session.messages.len(), 3);

        // Restore
        ctx.restore_state(&mut session, &snap).unwrap();
        assert_eq!(session.messages.len(), 2);
    }
}
