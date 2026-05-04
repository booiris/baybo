use std::sync::Arc;

use async_trait::async_trait;
use aura_model::{ChatMessage, ContentBlock, Role};

use super::{
    CompressOutput, CompressionStrategy, SummarizeCallback, SummarizeOutput, pair_preserving_cut,
};
use crate::tokenizer::Tokenizer;

/// Summarize compression: summarizes old non-system messages via an LLM
/// callback, then keeps the summary plus the most recent `keep_recent`
/// non-system messages.
///
/// Compared to `Truncate` (which simply drops old messages), `Summarize`
/// preserves the semantic content of earlier conversation turns in a
/// condensed form.
pub struct Summarize {
    callback: Arc<dyn SummarizeCallback>,
    keep_recent: usize,
}

impl Summarize {
    pub fn new(callback: Arc<dyn SummarizeCallback>, keep_recent: usize) -> Self {
        Self {
            callback,
            keep_recent,
        }
    }
}

#[async_trait]
impl CompressionStrategy for Summarize {
    async fn compress(
        &self,
        messages: &[ChatMessage],
        _tokenizer: &dyn Tokenizer,
    ) -> crate::Result<CompressOutput> {
        let mut system_msgs = Vec::new();
        let mut non_system = Vec::new();
        for msg in messages {
            if msg.role == Role::System {
                system_msgs.push(msg.clone());
            } else {
                non_system.push(msg.clone());
            }
        }

        if non_system.len() <= self.keep_recent {
            return Ok(CompressOutput {
                messages: messages.to_vec(),
                llm_call: None,
            });
        }

        // Split non-system messages into old (to summarize) and recent
        // (to keep). The initial split by `keep_recent` may land between
        // an `assistant { tool_use }` and the following
        // `user { tool_result }`; pull the boundary left until every
        // kept `ToolResult` has its `ToolUse` in the recent half so the
        // LLM payload remains well-formed.
        let initial_split = non_system.len().saturating_sub(self.keep_recent);
        let split = pair_preserving_cut(&non_system, initial_split);
        let old = &non_system[..split];
        let recent = &non_system[split..];

        // Summarize old messages via the injected callback.
        let SummarizeOutput { summary, llm_call } = self.callback.summarize(old).await?;

        // Build new message list: system + summary + recent.
        let mut new_messages = system_msgs;
        new_messages.push(ChatMessage {
            role: Role::System,
            content: vec![ContentBlock::Text(format!(
                "[Conversation Summary]\n{summary}"
            ))],
        });
        new_messages.extend_from_slice(recent);

        Ok(CompressOutput {
            messages: new_messages,
            llm_call: Some(llm_call),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoSummarizer;

    #[async_trait]
    impl SummarizeCallback for EchoSummarizer {
        async fn summarize(&self, messages: &[ChatMessage]) -> crate::Result<SummarizeOutput> {
            Ok(SummarizeOutput {
                summary: format!("Summary of {} messages", messages.len()),
                llm_call: crate::CompressionLlmCall {
                    model_id: "test-model".into(),
                    provider: "test".into(),
                    input_tokens: 0,
                    output_tokens: 0,
                },
            })
        }
    }

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
    async fn summarizes_old_messages() {
        let strategy = Summarize::new(Arc::new(EchoSummarizer), 2);
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

        // system + summary + 2 recent non-system
        assert_eq!(output.messages.len(), 4);
        assert_eq!(output.messages[0].role, Role::System); // original system
        assert_eq!(output.messages[1].role, Role::System); // summary

        if let ContentBlock::Text(ref text) = output.messages[1].content[0] {
            assert!(text.contains("Summary of 3 messages"));
        } else {
            panic!("expected text content in summary message");
        }
    }

    #[tokio::test]
    async fn no_change_when_under_keep_recent() {
        let strategy = Summarize::new(Arc::new(EchoSummarizer), 10);
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
