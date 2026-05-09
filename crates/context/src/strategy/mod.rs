pub mod summarize;
pub mod summary_aware;
pub mod truncate;

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use aura_llm::{ChatRequest, LlmResponse};
use aura_model::{ChatMessage, ContentBlock};

use crate::error::ContextError;

/// Future returned by a [`ChatCallback`]. Boxed because the callback
/// is passed through a `dyn`-compatible trait method.
pub type ChatFuture =
    Pin<Box<dyn Future<Output = std::result::Result<LlmResponse, ContextError>> + Send>>;

/// One-shot chat invocation handed to a [`CompressionStrategy`].
///
/// The strategy calls it at most once. Strategies that don't need an
/// LLM call (e.g. [`crate::Truncate`]) ignore it; strategies that do
/// (e.g. [`crate::Summarize`]) drive the call themselves so the chat
/// → trim → fallback flow stays inside the strategy. `'static` because
/// the agent loop's runner moves its captures by value.
pub type ChatCallback = Box<dyn FnOnce(ChatRequest) -> ChatFuture + Send>;

/// Walk backwards from `messages.len()` in atomic units, returning
/// the cut index where the kept tail satisfies the `min_tokens` and
/// `min_text_block_msgs` minima without exceeding `max_tokens`.
///
/// An atomic unit is either a single message or a `tool_use` /
/// `tool_result` pair treated as a unit; pair preservation is
/// strict — adding the next unit is rejected if it would push past
/// `max_tokens`, rather than pulling more messages in. Returns
/// `messages.len()` (empty kept slice) when even the first unit
/// exceeds the cap.
pub(crate) fn walk_backward_atomic<F>(
    messages: &[ChatMessage],
    min_tokens: usize,
    min_text_block_msgs: usize,
    max_tokens: usize,
    tokenize: F,
) -> usize
where
    F: Fn(&ChatMessage) -> usize,
{
    let mut cursor = messages.len();
    let mut tokens: usize = 0;
    let mut text_block_msgs: usize = 0;

    while cursor > 0 {
        // Pair-closing fixed-point: expand backward from cursor-1 to
        // include any earlier `tool_use` whose `tool_result` is
        // already in the kept tail.
        let new_cursor = pair_preserving_cut(messages, cursor - 1);
        let unit = &messages[new_cursor..cursor];
        let unit_tokens: usize = unit.iter().map(&tokenize).sum();

        if tokens + unit_tokens > max_tokens {
            break;
        }

        cursor = new_cursor;
        tokens += unit_tokens;
        text_block_msgs += unit
            .iter()
            .filter(|m| m.content.iter().any(|b| matches!(b, ContentBlock::Text(_))))
            .count();

        if tokens >= min_tokens && text_block_msgs >= min_text_block_msgs {
            break;
        }
    }

    cursor
}

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
/// Implementations receive the full message list and a one-shot chat
/// callback they can invoke if they need to call an LLM (e.g.
/// `Summarize`). The callback hides trace span / cost / security
/// wiring on the agent side — the strategy just sees
/// `request → LlmResponse`. Strategies that don't need an LLM
/// (e.g. `Truncate`) ignore the callback. Token counting is owned
/// by `ContextManager` and not exposed here; if a future strategy
/// needs token-aware decisions, plumb a tokenizer in alongside
/// `messages` rather than reviving a blanket trait parameter.
#[async_trait]
pub trait CompressionStrategy: Send + Sync {
    async fn compress(
        &self,
        messages: &[ChatMessage],
        chat: ChatCallback,
    ) -> crate::Result<CompressOutput>;
}

/// Result of a [`CompressionStrategy::compress`] call.
pub enum CompressOutput {
    /// Strategy chose not to compress (e.g. message count already at
    /// or below the keep threshold). `ContextManager` surfaces this
    /// as `CompressionOutcome::StrategyDeclined` without touching
    /// `session`.
    NoOp,
    /// Strategy produced a new message list.
    ///
    /// `replaced_full_history` declares whether this output discards
    /// the prior conversation in favour of an opaque condensation
    /// (e.g. a summary message that no longer carries the original
    /// `ToolUse` blocks). `ContextManager` uses the flag to decide
    /// whether to append a fresh skill trailer (reminder + per-skill
    /// detail blocks) — necessary after a full-history wipe so the
    /// model still sees the authoritative skill list, redundant after
    /// a tail-preserving truncation that already kept the historical
    /// reminder + tool_use trail in the kept slice.
    Replaced {
        messages: Vec<ChatMessage>,
        replaced_full_history: bool,
    },
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

    // ---------- walk_backward_atomic tests ----------

    /// Every text message counts as 10 tokens for these tests so the
    /// arithmetic stays obvious; tool_use / tool_result blocks count
    /// as 5 each (representing structural overhead).
    fn flat_tokenize(msg: &ChatMessage) -> usize {
        msg.content
            .iter()
            .map(|b| match b {
                ContentBlock::Text(_) => 10,
                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => 5,
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn walk_returns_empty_when_message_list_empty() {
        let msgs: Vec<ChatMessage> = Vec::new();
        assert_eq!(walk_backward_atomic(&msgs, 10, 1, 100, flat_tokenize), 0);
    }

    #[test]
    fn walk_satisfies_both_minima_then_stops() {
        // 6 text messages × 10 tokens each = 60 tokens total.
        let msgs: Vec<ChatMessage> = (0..6).map(|i| text(Role::User, &format!("m{i}"))).collect();
        // min_tokens = 25, min_text_blocks = 3, max = 100.
        // After 1 msg: 10 / 1 — not yet.
        // After 2 msgs: 20 / 2 — not yet.
        // After 3 msgs: 30 / 3 — both satisfied, stop.
        let cut = walk_backward_atomic(&msgs, 25, 3, 100, flat_tokenize);
        assert_eq!(cut, 3); // last 3 messages kept
    }

    #[test]
    fn walk_keeps_walking_when_only_text_blocks_min_satisfied_but_not_tokens() {
        // 6 messages; even if 3 messages have text, we keep walking
        // until tokens ≥ min_tokens too.
        let msgs: Vec<ChatMessage> = (0..6).map(|i| text(Role::User, &format!("m{i}"))).collect();
        // min_tokens = 50 (5 msgs needed at 10 each), min_text_blocks = 1.
        let cut = walk_backward_atomic(&msgs, 50, 1, 100, flat_tokenize);
        assert_eq!(cut, 1); // last 5 messages
    }

    #[test]
    fn walk_hard_cap_drops_unit_that_overflows() {
        // 5 text messages × 10 tokens = 50.
        // max_tokens = 25. After accepting 2 messages (20 tokens), the
        // next unit would push us to 30 > 25 → stop without taking it.
        let msgs: Vec<ChatMessage> = (0..5).map(|i| text(Role::User, &format!("m{i}"))).collect();
        let cut = walk_backward_atomic(&msgs, 1000, 1000, 25, flat_tokenize);
        assert_eq!(cut, 3); // last 2 messages
    }

    #[test]
    fn walk_returns_full_length_when_first_unit_exceeds_max() {
        // Single message at 10 tokens; cap at 5 → can't take it.
        let msgs = vec![text(Role::User, "big")];
        let cut = walk_backward_atomic(&msgs, 1, 1, 5, flat_tokenize);
        assert_eq!(cut, 1); // empty kept slice
    }

    #[test]
    fn walk_treats_tool_use_pair_atomically() {
        // [user "ask", assistant{tool_use:tu1}, user{tool_result:tu1}]
        // Walking from end: candidate = user{tool_result:tu1}
        // pair_preserving_cut pulls in tool_use at index 1.
        // Unit = messages[1..3] = 2 msgs, 5+5=10 tokens.
        let msgs = vec![text(Role::User, "ask"), tool_use("tu1"), tool_result("tu1")];
        let cut = walk_backward_atomic(&msgs, 1, 0, 100, flat_tokenize);
        // Took the pair atomically → cut at index 1, kept slice = [tool_use, tool_result]
        // min_tokens=1 satisfied at 10 tokens; min_text_blocks=0 satisfied trivially. Stop.
        assert_eq!(cut, 1);
    }

    #[test]
    fn walk_drops_atomic_pair_that_exceeds_cap() {
        // [user "ask", assistant{tool_use:tu1}, user{tool_result:tu1}]
        // The pair unit costs 10 tokens; cap = 5 → can't take it.
        let msgs = vec![text(Role::User, "ask"), tool_use("tu1"), tool_result("tu1")];
        let cut = walk_backward_atomic(&msgs, 1, 0, 5, flat_tokenize);
        // Strict cap → empty kept slice, cut = messages.len() = 3.
        assert_eq!(cut, 3);
    }

    #[test]
    fn walk_takes_everything_when_minima_unreachable() {
        // 3 text messages × 10 tokens = 30. min_tokens = 100 → never
        // satisfied; loop runs to cursor=0.
        let msgs: Vec<ChatMessage> = (0..3).map(|i| text(Role::User, &format!("m{i}"))).collect();
        let cut = walk_backward_atomic(&msgs, 100, 100, 1_000, flat_tokenize);
        assert_eq!(cut, 0);
    }
}
