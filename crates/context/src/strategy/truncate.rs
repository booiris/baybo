use async_trait::async_trait;
use aura_model::ChatMessage;
#[cfg(test)]
use aura_model::Role;

use super::{
    ChatCallback, CompressOutput, CompressionStrategy, pair_preserving_cut, partition_system,
};

/// Truncation compression: keeps system messages and the most recent
/// `keep_recent` non-system messages, discarding everything in between.
pub struct Truncate {
    keep_recent: usize,
}

impl Truncate {
    pub fn new(keep_recent: usize) -> Self {
        Self { keep_recent }
    }
}

#[async_trait]
impl CompressionStrategy for Truncate {
    async fn compress(
        &self,
        messages: &[ChatMessage],
        _chat: ChatCallback,
    ) -> crate::Result<CompressOutput> {
        let (system_msgs, non_system) = partition_system(messages);

        if non_system.len() <= self.keep_recent {
            return Ok(CompressOutput::NoOp);
        }

        // Pull the cut leftward as needed so every kept `ToolResult`
        // still has its originating `ToolUse` in the tail. Otherwise
        // the LLM payload is malformed.
        let initial_cut = non_system.len().saturating_sub(self.keep_recent);
        let kept_start = pair_preserving_cut(&non_system, initial_cut);
        let kept: Vec<ChatMessage> = non_system[kept_start..].to_vec();

        let mut new_messages = system_msgs;
        new_messages.extend(kept);

        Ok(CompressOutput::Replaced {
            messages: new_messages,
            replaced_full_history: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::ContentBlock;

    fn make_msg(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![ContentBlock::Text(text.to_string())],
        }
    }

    /// Truncate ignores the chat callback. Asserts via a panic-on-call
    /// stub so a future regression that wires Truncate into the LLM
    /// path can't slip past.
    fn never_chat() -> ChatCallback {
        Box::new(|_req| {
            Box::pin(async move {
                panic!("Truncate must not invoke the chat callback");
            })
        })
    }

    #[tokio::test]
    async fn keeps_system_and_recent() {
        let strategy = Truncate::new(2);

        let messages = vec![
            make_msg(Role::System, "system prompt"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        match strategy.compress(&messages, never_chat()).await.unwrap() {
            CompressOutput::Replaced {
                messages: new_messages,
                replaced_full_history,
            } => {
                assert_eq!(new_messages.len(), 3);
                assert_eq!(new_messages[0].role, Role::System);
                assert!(!replaced_full_history, "Truncate keeps the tail in place");
            }
            _ => panic!("expected Replaced"),
        }
    }

    #[tokio::test]
    async fn no_op_when_under_keep_recent() {
        let strategy = Truncate::new(10);

        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "hello"),
            make_msg(Role::Assistant, "hi"),
        ];

        match strategy.compress(&messages, never_chat()).await.unwrap() {
            CompressOutput::NoOp => {}
            _ => panic!("expected NoOp"),
        }
    }
}
