pub mod budget;
pub mod error;
pub mod snapshot;
pub mod strategy;
pub mod tokenizer;

pub use budget::TokenBudget;
pub use error::ContextError;
pub use snapshot::ContextSnapshot;
pub use strategy::CompressionStrategy;
pub use strategy::SummarizeCallback;
pub use strategy::summarize::Summarize;
pub use strategy::truncate::Truncate;
pub use tokenizer::Tokenizer;

pub type Result<T> = std::result::Result<T, ContextError>;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_model::ChatMessage;
use aura_session::Session;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Statistics from a compression operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressStats {
    /// Token count before compression.
    pub before_tokens: usize,
    /// Token count after compression.
    pub after_tokens: usize,
    /// Wall-clock time spent on compression.
    pub latency: Duration,
}

/// Manages session context: appending messages with automatic compression,
/// token budget tracking, and snapshots.
///
/// This is a concrete struct, not a trait. The only extension point is the
/// `CompressionStrategy` injected at construction — the management logic
/// (append, budget check, snapshot) is invariant.
pub struct ContextManager {
    tokenizer: Arc<dyn Tokenizer>,
    strategy: Box<dyn CompressionStrategy>,
    budget: TokenBudget,
}

impl ContextManager {
    pub fn new(
        tokenizer: Arc<dyn Tokenizer>,
        strategy: Box<dyn CompressionStrategy>,
        budget: TokenBudget,
    ) -> Self {
        Self {
            tokenizer,
            strategy,
            budget,
        }
    }

    /// Append a message to the session context.
    ///
    /// Automatically triggers compression when the token budget threshold
    /// is exceeded. Returns compression statistics if compression occurred.
    pub async fn append(
        &mut self,
        session: &mut Session,
        msg: &ChatMessage,
    ) -> crate::Result<Option<CompressStats>> {
        session.messages.push(msg.clone());
        self.budget.update(self.count_tokens(&session.messages));

        if !self.budget.needs_compression() {
            return Ok(None);
        }

        let start = Instant::now();
        let before_tokens = self.budget.current();
        let before_len = session.messages.len();

        let output = self
            .strategy
            .compress(&session.messages, &*self.tokenizer)
            .await?;

        session.messages = output.messages;
        let after_tokens = self.count_tokens(&session.messages);
        self.budget.update(after_tokens);

        // Strategy couldn't reduce the message count (e.g. already at keep_recent)
        if session.messages.len() >= before_len {
            return Ok(None);
        }

        if after_tokens > self.budget.max_tokens() {
            warn!(
                after_tokens,
                max_tokens = self.budget.max_tokens(),
                "token count still exceeds max_tokens after compression"
            );
        }

        let stats = CompressStats {
            before_tokens,
            after_tokens,
            latency: start.elapsed(),
        };

        debug!(
            before = stats.before_tokens,
            after = stats.after_tokens,
            latency_ms = stats.latency.as_millis() as u64,
            "context compressed"
        );

        Ok(Some(stats))
    }

    /// Check the token budget and compress if the threshold is exceeded.
    ///
    /// Unlike `append()` which auto-compresses after adding a message, this
    /// method is designed for the top of the agent loop to proactively compress
    /// before building the next `ChatRequest`.
    pub async fn maybe_compress(
        &mut self,
        session: &mut Session,
    ) -> crate::Result<Option<CompressStats>> {
        self.budget.update(self.count_tokens(&session.messages));

        if !self.budget.needs_compression() {
            return Ok(None);
        }

        let start = Instant::now();
        let before_tokens = self.budget.current();
        let before_len = session.messages.len();

        let output = self
            .strategy
            .compress(&session.messages, &*self.tokenizer)
            .await?;

        session.messages = output.messages;
        let after_tokens = self.count_tokens(&session.messages);
        self.budget.update(after_tokens);

        if session.messages.len() >= before_len {
            return Ok(None);
        }

        if after_tokens > self.budget.max_tokens() {
            warn!(
                after_tokens,
                max_tokens = self.budget.max_tokens(),
                "token count still exceeds max_tokens after proactive compression"
            );
        }

        let stats = CompressStats {
            before_tokens,
            after_tokens,
            latency: start.elapsed(),
        };

        debug!(
            before = stats.before_tokens,
            after = stats.after_tokens,
            latency_ms = stats.latency.as_millis() as u64,
            "proactive context compression"
        );

        Ok(Some(stats))
    }

    /// Read-only access to the token budget.
    pub fn budget(&self) -> &TokenBudget {
        &self.budget
    }

    /// Create a snapshot of the current session context.
    pub fn snapshot(&self, session: &Session) -> ContextSnapshot {
        ContextSnapshot {
            messages: session.messages.clone(),
            token_count: self.budget.current(),
        }
    }

    /// Restore the session context from a previously captured snapshot.
    pub fn restore(
        &mut self,
        session: &mut Session,
        snapshot: &ContextSnapshot,
    ) -> crate::Result<()> {
        session.messages = snapshot.messages.clone();
        self.budget.update(snapshot.token_count);
        Ok(())
    }

    fn count_tokens(&self, messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|m| self.tokenizer.count_message(m))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ContentBlock, Role};

    struct SimpleTokenizer;

    impl Tokenizer for SimpleTokenizer {
        fn count_text(&self, text: &str) -> usize {
            text.len() / 4 + 1
        }
        fn count_image(&self, _w: u32, _h: u32) -> usize {
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

    fn make_ctx(keep_recent: usize, max_tokens: usize, threshold: f64) -> ContextManager {
        ContextManager::new(
            Arc::new(SimpleTokenizer),
            Box::new(Truncate::new(keep_recent)),
            TokenBudget::new(max_tokens, threshold),
        )
    }

    #[tokio::test]
    async fn append_adds_message_without_compression() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);

        let msg = make_msg(Role::User, "hello");
        let stats = ctx.append(&mut session, &msg).await.unwrap();

        assert!(stats.is_none());
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, Role::User);
    }

    #[tokio::test]
    async fn auto_compress_on_token_threshold() {
        // max=50, threshold=0.5 → compress when > 25 tokens
        let mut ctx = make_ctx(2, 50, 0.5);
        let mut session = make_session(vec![]);

        // Build up messages one by one
        ctx.append(&mut session, &make_msg(Role::System, "You are helpful"))
            .await
            .unwrap();
        ctx.append(&mut session, &make_msg(Role::User, "First message here"))
            .await
            .unwrap();
        ctx.append(&mut session, &make_msg(Role::Assistant, "First reply here"))
            .await
            .unwrap();

        // This append pushes past threshold and has enough messages to compress
        let stats = ctx
            .append(&mut session, &make_msg(Role::User, "Second message here"))
            .await
            .unwrap();

        assert!(stats.is_some());
        // system + 2 most recent non-system
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].role, Role::System);
    }

    #[tokio::test]
    async fn no_compress_under_threshold() {
        let mut ctx = make_ctx(10, 100_000, 0.75);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &make_msg(Role::System, "sys"))
            .await
            .unwrap();
        ctx.append(&mut session, &make_msg(Role::User, "hi"))
            .await
            .unwrap();
        let stats = ctx
            .append(&mut session, &make_msg(Role::Assistant, "hello"))
            .await
            .unwrap();

        assert!(stats.is_none());
        assert_eq!(session.messages.len(), 3);
    }

    #[tokio::test]
    async fn no_compress_when_already_at_keep_recent() {
        // Low threshold triggers compression check, but only 2 non-system
        // messages with keep_recent=5 → strategy can't reduce further.
        let mut ctx = make_ctx(5, 10, 0.1);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &make_msg(Role::System, "sys"))
            .await
            .unwrap();
        ctx.append(&mut session, &make_msg(Role::User, "hi"))
            .await
            .unwrap();
        let stats = ctx
            .append(&mut session, &make_msg(Role::Assistant, "hello"))
            .await
            .unwrap();

        assert!(stats.is_none());
        assert_eq!(session.messages.len(), 3);
    }

    #[tokio::test]
    async fn snapshot_and_restore() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &make_msg(Role::System, "sys"))
            .await
            .unwrap();
        ctx.append(&mut session, &make_msg(Role::User, "hello"))
            .await
            .unwrap();

        let snap = ctx.snapshot(&session);
        assert_eq!(snap.messages.len(), 2);
        assert!(snap.token_count > 0);

        // Mutate
        ctx.append(&mut session, &make_msg(Role::Assistant, "hi"))
            .await
            .unwrap();
        assert_eq!(session.messages.len(), 3);

        // Restore
        ctx.restore(&mut session, &snap).unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(ctx.budget().current(), snap.token_count);
    }

    #[tokio::test]
    async fn budget_tracks_tokens() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);

        assert_eq!(ctx.budget().current(), 0);

        ctx.append(&mut session, &make_msg(Role::User, "hello world"))
            .await
            .unwrap();

        assert!(ctx.budget().current() > 0);
        assert!(ctx.budget().remaining() < 100_000);
    }
}
