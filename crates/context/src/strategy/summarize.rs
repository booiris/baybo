use aura_llm::ChatRequest;
use aura_model::{ChatMessage, ContentBlock, Role};

use super::{CompressOutput, CompressionStrategy, pair_preserving_cut};
use crate::tokenizer::Tokenizer;

/// Trailing instruction appended to the messages handed to the
/// summarizer LLM. Lives here rather than in the agent loop because
/// it's the strategy that decides what shape of summary to ask for —
/// the agent loop just dispatches the request and returns the text.
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
/// The strategy itself never makes an LLM call. Instead it returns a
/// `CompressOutput::NeedsLlmCall` plan with the prepared `ChatRequest`
/// and the assembly closures; `ContextManager::maybe_compress` invokes
/// the caller-supplied chat closure to fulfil the plan, and the agent
/// loop wraps that closure in a real trace span and cost record.
pub struct Summarize {
    keep_recent: usize,
}

impl Summarize {
    pub fn new(keep_recent: usize) -> Self {
        Self { keep_recent }
    }
}

impl CompressionStrategy for Summarize {
    fn compress(
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

        // Failure fallback: a Truncate-equivalent slice (system + recent)
        // so a transient summarizer error never kills the user's turn.
        let mut on_failure = system_msgs.clone();
        on_failure.extend_from_slice(&recent);

        let on_success_system = system_msgs;
        let on_success_recent = recent;
        let on_success = Box::new(move |summary: String| {
            let mut new_messages = on_success_system;
            new_messages.push(ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::Text(format!(
                    "[Conversation Summary]\n{summary}"
                ))],
            });
            new_messages.extend(on_success_recent);
            new_messages
        });

        Ok(CompressOutput::NeedsLlmCall {
            request,
            on_success,
            on_failure,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Plan for a 6-message conversation with `keep_recent=2` should
    /// be `NeedsLlmCall` carrying a request that contains the 3 old
    /// non-system messages + the trailing instruction. `on_success`
    /// must produce `[system, summary-as-system, recent...]`.
    #[test]
    fn produces_needs_llm_call_with_old_messages() {
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

        match strategy.compress(&messages, &tokenizer).unwrap() {
            CompressOutput::NeedsLlmCall {
                request,
                on_success,
                on_failure,
            } => {
                // 3 old non-system messages + 1 instruction message
                assert_eq!(request.messages.len(), 4);
                assert!(request.tools.is_empty());
                assert!(request.temperature.is_none());

                let trailing = request.messages.last().unwrap();
                assert_eq!(trailing.role, Role::User);
                if let ContentBlock::Text(t) = &trailing.content[0] {
                    assert!(t.contains("summarizing the older portion"));
                } else {
                    panic!("expected text instruction");
                }

                // on_success → system + summary + 2 recent non-system
                let assembled = on_success("CANNED".into());
                assert_eq!(assembled.len(), 4);
                assert_eq!(assembled[0].role, Role::System);
                assert_eq!(assembled[1].role, Role::System);
                if let ContentBlock::Text(t) = &assembled[1].content[0] {
                    assert!(t.contains("[Conversation Summary]"));
                    assert!(t.contains("CANNED"));
                } else {
                    panic!("expected summary text");
                }

                // on_failure → system + 2 recent non-system, no summary
                assert_eq!(on_failure.len(), 3);
                assert_eq!(on_failure[0].role, Role::System);
                if let ContentBlock::Text(t) = &on_failure[1].content[0] {
                    assert_eq!(t, "reply 2");
                } else {
                    panic!("expected text content");
                }
            }
            other => panic!("expected NeedsLlmCall, got: {:?}", variant_name(&other)),
        }
    }

    #[test]
    fn no_op_when_under_keep_recent() {
        let strategy = Summarize::new(10);
        let tokenizer = SimpleTokenizer;
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "hello"),
            make_msg(Role::Assistant, "hi"),
        ];

        match strategy.compress(&messages, &tokenizer).unwrap() {
            CompressOutput::NoOp => {}
            other => panic!("expected NoOp, got: {:?}", variant_name(&other)),
        }
    }

    /// `pair_preserving_cut` can drag the boundary all the way to 0 if
    /// the head's tool_uses are only resolved by tool_results in the
    /// tail. The strategy must NoOp instead of paying for a summarizer
    /// call on an empty old slice.
    #[test]
    fn no_op_when_pair_cut_collapses_to_zero() {
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

        match strategy.compress(&messages, &tokenizer).unwrap() {
            CompressOutput::NoOp => {}
            other => panic!("expected NoOp, got: {:?}", variant_name(&other)),
        }
    }

    fn variant_name(o: &CompressOutput) -> &'static str {
        match o {
            CompressOutput::NoOp => "NoOp",
            CompressOutput::Replaced(_) => "Replaced",
            CompressOutput::NeedsLlmCall { .. } => "NeedsLlmCall",
        }
    }
}
