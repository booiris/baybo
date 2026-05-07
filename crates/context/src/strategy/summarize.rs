use async_trait::async_trait;
use aura_llm::ChatRequest;
use aura_model::{ChatMessage, ContentBlock, Role};
use tracing::warn;

use super::{ChatCallback, CompressOutput, CompressionStrategy, pair_preserving_cut};
use crate::tokenizer::Tokenizer;

/// Trailing instruction appended to the messages handed to the
/// summarizer LLM. Lives here rather than in the agent loop because
/// it's the strategy that decides what shape of summary to ask for —
/// the agent loop just runs the call.
const SUMMARIZE_INSTRUCTION: &str = "\
You are summarizing the older portion of an agent's own conversation \
so it can continue the same task. Preserve: the user's current request \
and any constraints; recent tool calls and the key facts they returned \
(file paths, IDs, error messages); decisions already made; open todos; \
anything the agent must remember to finish the task. Drop: redundant \
exchanges, exploratory dead-ends. Output plain prose, no preamble.";

/// Summarize compression: condenses old non-system messages into a
/// single `[Conversation Summary]` block and keeps the most recent
/// `keep_recent` non-system messages alongside it.
///
/// Drives the LLM call itself via the [`ChatCallback`] passed in by
/// `ContextManager::maybe_compress`: builds the request, invokes the
/// callback, trims the response, and either assembles the new
/// message list or falls back to a Truncate-equivalent slice on
/// failure / empty content. The callback is where the agent loop
/// wraps the call in a real trace span and cost record.
pub struct Summarize {
    keep_recent: usize,
}

impl Summarize {
    pub fn new(keep_recent: usize) -> Self {
        Self { keep_recent }
    }
}

#[async_trait]
impl CompressionStrategy for Summarize {
    async fn compress(
        &self,
        messages: &[ChatMessage],
        _tokenizer: &dyn Tokenizer,
        chat: ChatCallback,
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
            return Ok(CompressOutput::NoOp);
        }

        // Pull the boundary left if it would sever a tool_use /
        // tool_result pair, otherwise the LLM payload is malformed.
        let initial_split = non_system.len().saturating_sub(self.keep_recent);
        let split = pair_preserving_cut(&non_system, initial_split);
        // `pair_preserving_cut` can drag the boundary all the way to 0
        // when the head of the conversation is a single tool_use /
        // tool_result chain. There's nothing to summarise in that case
        // — bail before paying for an LLM call on an empty old slice.
        if split == 0 {
            return Ok(CompressOutput::NoOp);
        }
        let old: Vec<ChatMessage> = non_system[..split].to_vec();
        let recent: Vec<ChatMessage> = non_system[split..].to_vec();

        let mut request_messages = old;
        request_messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(SUMMARIZE_INSTRUCTION.to_string())],
        });
        let request = ChatRequest {
            messages: request_messages,
            temperature: None,
            tools: Vec::new(),
        };

        // Deterministic fallback used on transport / sanitize failure
        // or empty summary: a Truncate-equivalent slice (system +
        // recent) so a transient summarizer error never kills the
        // user's turn.
        let fallback = || {
            let mut out = system_msgs.clone();
            out.extend_from_slice(&recent);
            out
        };

        match chat(request).await {
            Ok(response) => {
                let summary = response.content.trim().to_string();
                if summary.is_empty() {
                    warn!("summarizer returned empty content; falling back to truncation");
                    return Ok(CompressOutput::Replaced(fallback()));
                }
                let mut new_messages = system_msgs;
                new_messages.push(ChatMessage {
                    role: Role::System,
                    content: vec![ContentBlock::Text(format!(
                        "[Conversation Summary]\n{summary}"
                    ))],
                });
                new_messages.extend(recent);
                Ok(CompressOutput::Replaced(new_messages))
            }
            Err(e) => {
                warn!(error = %e, "summarization failed; falling back to truncation");
                Ok(CompressOutput::Replaced(fallback()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_llm::{LlmResponse, TokenUsage};

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

    fn ok_chat(content: &'static str) -> ChatCallback {
        Box::new(move |_req| {
            Box::pin(async move {
                Ok(LlmResponse {
                    content: content.to_string(),
                    content_blocks: vec![],
                    tool_calls: vec![],
                    usage: TokenUsage::default(),
                    thinking: None,
                })
            })
        })
    }

    fn err_chat() -> ChatCallback {
        Box::new(|_req| {
            Box::pin(async move { Err(crate::error::ContextError::Compression("boom".into())) })
        })
    }

    fn never_chat() -> ChatCallback {
        Box::new(|_req| {
            Box::pin(async move {
                panic!("chat must not be invoked when strategy NoOps");
            })
        })
    }

    /// 6 non-system + 1 system, `keep_recent=2`. `chat` returns
    /// "CANNED"; the resulting Replaced list must be
    /// `[system, summary-as-system, recent...]` (2 recent + 1 summary
    /// + 1 original system = 4 entries).
    #[tokio::test]
    async fn produces_replaced_with_summary_and_recent() {
        let strategy = Summarize::new(2);
        let tokenizer = SimpleTokenizer;
        let messages = vec![
            make_msg(Role::System, "system prompt"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        match strategy
            .compress(&messages, &tokenizer, ok_chat("CANNED"))
            .await
            .unwrap()
        {
            CompressOutput::Replaced(new_messages) => {
                assert_eq!(new_messages.len(), 4);
                assert_eq!(new_messages[0].role, Role::System);
                assert_eq!(new_messages[1].role, Role::System);
                if let ContentBlock::Text(t) = &new_messages[1].content[0] {
                    assert!(t.contains("[Conversation Summary]"));
                    assert!(t.contains("CANNED"));
                } else {
                    panic!("expected summary text");
                }
            }
            _ => panic!("expected Replaced"),
        }
    }

    /// On chat error the strategy must fall back to a Truncate-equivalent
    /// slice: `[system, recent...]` — no summary block.
    #[tokio::test]
    async fn chat_error_falls_back_to_truncation() {
        let strategy = Summarize::new(2);
        let tokenizer = SimpleTokenizer;
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        match strategy
            .compress(&messages, &tokenizer, err_chat())
            .await
            .unwrap()
        {
            CompressOutput::Replaced(new_messages) => {
                assert_eq!(new_messages.len(), 3);
                assert_eq!(new_messages[0].role, Role::System);
                if let ContentBlock::Text(t) = &new_messages[1].content[0] {
                    assert_eq!(t, "reply 2");
                } else {
                    panic!("expected text content");
                }
            }
            _ => panic!("expected Replaced fallback"),
        }
    }

    /// Empty / whitespace-only summary takes the same fallback path
    /// as a transport error.
    #[tokio::test]
    async fn empty_summary_falls_back_to_truncation() {
        let strategy = Summarize::new(2);
        let tokenizer = SimpleTokenizer;
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        match strategy
            .compress(&messages, &tokenizer, ok_chat("   \n  "))
            .await
            .unwrap()
        {
            CompressOutput::Replaced(new_messages) => {
                // No summary block — fallback shape.
                assert_eq!(new_messages.len(), 3);
                if let ContentBlock::Text(t) = &new_messages[1].content[0] {
                    assert_eq!(t, "reply 2");
                } else {
                    panic!("expected text content");
                }
            }
            _ => panic!("expected Replaced fallback"),
        }
    }

    #[tokio::test]
    async fn no_op_when_under_keep_recent() {
        let strategy = Summarize::new(10);
        let tokenizer = SimpleTokenizer;
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "hello"),
            make_msg(Role::Assistant, "hi"),
        ];

        match strategy
            .compress(&messages, &tokenizer, never_chat())
            .await
            .unwrap()
        {
            CompressOutput::NoOp => {}
            _ => panic!("expected NoOp"),
        }
    }

    /// `pair_preserving_cut` can drag the boundary all the way to 0 if
    /// the head's tool_uses are only resolved by tool_results in the
    /// tail. The strategy must NoOp instead of paying for a summarizer
    /// call on an empty old slice.
    #[tokio::test]
    async fn no_op_when_pair_cut_collapses_to_zero() {
        let strategy = Summarize::new(1);
        let tokenizer = SimpleTokenizer;
        let tool_use = ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "a".into(),
                name: "bash".into(),
                input: serde_json::Value::Null,
                signature: None,
            }],
        };
        let tool_result = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "a".into(),
                content: "ok".into(),
            }],
        };
        let messages = vec![
            make_msg(Role::System, "system"),
            tool_use,
            make_msg(Role::User, "interleaved text"),
            tool_result,
        ];

        match strategy
            .compress(&messages, &tokenizer, never_chat())
            .await
            .unwrap()
        {
            CompressOutput::NoOp => {}
            _ => panic!("expected NoOp"),
        }
    }
}
