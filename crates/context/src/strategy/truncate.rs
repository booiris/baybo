use async_trait::async_trait;
use aura_model::{ChatMessage, Role};

use super::{CompressOutput, CompressionStrategy};
use crate::tokenizer::Tokenizer;

/// Truncation compression: keeps system messages and the most recent
/// `keep_recent` non-system messages, discarding everything in between.
pub struct Truncate {
    keep_recent: usize,
}

impl Truncate {
    pub fn new(keep_recent: usize) -> Self {
        Self { keep_recent }
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
impl CompressionStrategy for Truncate {
    async fn compress(
        &self,
        messages: &[ChatMessage],
        _tokenizer: &dyn Tokenizer,
    ) -> crate::Result<CompressOutput> {
        let (system_msgs, non_system) = Self::partition_system(messages);

        if non_system.len() <= self.keep_recent {
            return Ok(CompressOutput {
                messages: messages.to_vec(),
            });
        }

        let kept_start = non_system.len().saturating_sub(self.keep_recent);
        let kept: Vec<ChatMessage> = non_system[kept_start..].to_vec();

        let mut new_messages = system_msgs;
        new_messages.extend(kept);

        Ok(CompressOutput {
            messages: new_messages,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::ContentBlock;

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

    #[tokio::test]
    async fn keeps_system_and_recent() {
        let strategy = Truncate::new(2);
        let tokenizer = SimpleTokenizer;

        let messages = vec![
            make_msg(Role::System, "system prompt"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        let output = strategy.compress(&messages, &tokenizer).await.unwrap();
        assert_eq!(output.messages.len(), 3); // system + 2 recent
        assert_eq!(output.messages[0].role, Role::System);
    }

    #[tokio::test]
    async fn no_change_when_under_keep_recent() {
        let strategy = Truncate::new(10);
        let tokenizer = SimpleTokenizer;

        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "hello"),
            make_msg(Role::Assistant, "hi"),
        ];

        let output = strategy.compress(&messages, &tokenizer).await.unwrap();
        assert_eq!(output.messages.len(), 3);
    }
}
