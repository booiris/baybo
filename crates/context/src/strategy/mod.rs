pub mod summarize;
pub mod truncate;

use async_trait::async_trait;
use aura_model::{ChatMessage, ContentBlock};
use std::collections::HashSet;

use crate::tokenizer::Tokenizer;

/// Adjust a candidate cut index over `messages` so the kept tail
/// (`messages[cut..]`) contains every `ToolUse` whose matching
/// `ToolResult` is in the tail. Returning a smaller index than the
/// caller's first guess is the only direction this function moves —
/// we never drop more, only pull additional `ToolUse` blocks back in.
///
/// Anthropic / OpenAI both reject arrays where a `tool_use_id` shows
/// up on the result side without the originating `tool_use`, so
/// truncation strategies that split on a fixed `keep_recent` boundary
/// must call this before slicing. Without it a cut that lands between
/// `assistant { tool_use }` and the following `user { tool_result }`
/// silently corrupts the LLM payload.
pub(crate) fn pair_preserving_cut(messages: &[ChatMessage], cut: usize) -> usize {
    let mut new_cut = cut.min(messages.len());
    if new_cut == 0 {
        return 0;
    }

    // Fixed-point: every iteration recomputes the unmet `tool_use_id`s
    // in the current tail (`messages[new_cut..]`) and pulls the cut
    // leftward to the first message that supplies one of them. Pulling
    // a message in may bring fresh `ToolResult` blocks along, which is
    // why a single pass isn't enough.
    loop {
        let mut needed: HashSet<&str> = HashSet::new();
        for msg in &messages[new_cut..] {
            for block in &msg.content {
                if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                    needed.insert(tool_use_id.as_str());
                }
            }
        }
        for msg in &messages[new_cut..] {
            for block in &msg.content {
                if let ContentBlock::ToolUse { id, .. } = block {
                    needed.remove(id.as_str());
                }
            }
        }
        if needed.is_empty() {
            return new_cut;
        }

        let mut moved = false;
        'scan: for i in (0..new_cut).rev() {
            for block in &messages[i].content {
                if let ContentBlock::ToolUse { id, .. } = block
                    && needed.contains(id.as_str())
                {
                    new_cut = i;
                    moved = true;
                    break 'scan;
                }
            }
        }
        if !moved {
            // No earlier `ToolUse` matches the orphaned `ToolResult`
            // ids. The originating call must have been outside the
            // input window entirely; the caller will see a payload
            // with orphan results and the LLM will reject it. We've
            // pulled back as much as we can.
            return new_cut;
        }
    }
}

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
    /// Set when the strategy made an LLM call. Propagated upward by
    /// `ContextManager` so the agent loop can record the call as a
    /// `SpanKind::LlmCall` after the fact (see `CompressStats::llm_call`).
    pub llm_call: Option<crate::CompressionLlmCall>,
}

/// Output of [`SummarizeCallback::summarize`].
///
/// Carries both the summary text and the provenance/usage of the
/// underlying LLM call so the strategy can surface it via
/// `CompressOutput::llm_call`.
pub struct SummarizeOutput {
    pub summary: String,
    pub llm_call: crate::CompressionLlmCall,
}

/// Callback for LLM-based summarization of context messages.
///
/// Defined in this crate but implemented externally to keep `context`
/// independent from `llm`. Injected into `Summarize` strategy at construction.
#[async_trait]
pub trait SummarizeCallback: Send + Sync {
    /// Summarize a sequence of messages, returning both the text and
    /// the LLM call's provenance/usage.
    async fn summarize(&self, messages: &[ChatMessage]) -> crate::Result<SummarizeOutput>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::Role;

    fn tool_use(id: &str) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "bash".into(),
                input: serde_json::Value::Null,
                signature: None,
            }],
        }
    }

    fn tool_result(id: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
            }],
        }
    }

    fn text(role: Role, t: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![ContentBlock::Text(t.into())],
        }
    }

    #[test]
    fn cut_between_tool_use_and_result_pulls_back() {
        // Indices: 0:user 1:assistant{tool_use:tu1} 2:user{tool_result:tu1} 3:assistant
        let msgs = vec![
            text(Role::User, "ask"),
            tool_use("tu1"),
            tool_result("tu1"),
            text(Role::Assistant, "done"),
        ];
        // Caller wanted to keep the last 2 (cut=2), which would orphan
        // the tool_result at index 2. Adjusted cut must include the
        // tool_use at index 1.
        assert_eq!(pair_preserving_cut(&msgs, 2), 1);
    }

    #[test]
    fn cut_with_clean_boundary_unchanged() {
        let msgs = vec![
            tool_use("tu1"),
            tool_result("tu1"),
            text(Role::User, "next"),
            text(Role::Assistant, "reply"),
        ];
        // Cut at 2 cleanly drops a complete tool exchange; nothing to pull back.
        assert_eq!(pair_preserving_cut(&msgs, 2), 2);
    }

    #[test]
    fn dangling_tool_result_pulls_back_through_intermediate_messages() {
        // Tool exchange straddled by other turns: tu1 at 1, result at 4.
        let msgs = vec![
            text(Role::User, "earlier"),
            tool_use("tu1"),
            text(Role::Assistant, "thinking"),
            text(Role::User, "still"),
            tool_result("tu1"),
        ];
        assert_eq!(pair_preserving_cut(&msgs, 4), 1);
    }

    #[test]
    fn multiple_tool_uses_all_paired() {
        let msgs = vec![
            tool_use("tu1"),
            tool_use("tu2"),
            tool_result("tu1"),
            tool_result("tu2"),
            text(Role::Assistant, "done"),
        ];
        // Cut=3 keeps tu2-result and one assistant turn; tu1-use and
        // tu2-use both need to be preserved → must move to 0.
        assert_eq!(pair_preserving_cut(&msgs, 3), 0);
    }

    #[test]
    fn cut_zero_or_full_is_noop() {
        let msgs = vec![tool_use("tu1"), tool_result("tu1")];
        assert_eq!(pair_preserving_cut(&msgs, 0), 0);
        assert_eq!(pair_preserving_cut(&msgs, 2), 2);
        assert_eq!(pair_preserving_cut(&msgs, 99), 2);
    }
}
