//! Hardcoded 3-stage context compression flow.
//!
//! 1. **summary.md fast-path**: when a precomputed summary is available
//!    on disk and its assembly fits within
//!    `FAST_PATH_FALLTHROUGH_THRESHOLD_RATIO × max_tokens`, swap in
//!    `[system + summary blob + recent slice]` without an LLM call.
//! 2. **Live LLM summary**: send the full conversation +
//!    [`SUMMARIZE_INSTRUCTION`] to the model, replace the transcript
//!    with `[system + parsed summary]`.
//! 3. **Truncate fallback**: when the LLM call fails or returns no
//!    usable content, keep `system + last keep_recent non-system`
//!    messages (pair-preserving so tool_use / tool_result stays intact).
//!
//! When the conversation is already at or below `keep_recent`
//! non-system messages, the flow returns [`CompressOutput::NoOp`]
//! without firing the LLM — even the truncate fallback couldn't
//! shrink it.
//!
//! See `docs/background-compression.md` for the trigger conditions
//! and design rationale.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use aura_llm::{ChatRequest, LlmResponse};
use aura_model::{ChatMessage, ContentBlock, Role};
use tracing::{debug, warn};

use crate::error::ContextError;
use crate::{
    ContextManager, FAST_PATH_FALLTHROUGH_THRESHOLD_RATIO, RECENT_SLICE_MAX_TOKENS,
    RECENT_SLICE_MIN_TEXT_BLOCK_MSGS, RECENT_SLICE_MIN_TOKENS, SUMMARY_REFRESH_WAIT_POLL_INTERVAL,
    SUMMARY_REFRESH_WAIT_TIMEOUT, estimate_skill_trailer_tokens, scan_skill_calls,
};

const CONTEXT_SUMMARY_WRAPPER_PREAMBLE: &str = "The conversation prior to this point has been compressed for context-window \
management. The summary below was produced from the full prior conversation and \
represents its substantive content. Treat it as established context for the user's \
current request; the recent messages that follow are the only unsummarized exchanges.";

pub type ChatFuture =
    Pin<Box<dyn Future<Output = std::result::Result<LlmResponse, ContextError>> + Send>>;

/// One-shot chat invocation handed to the compressor. Invoked at most
/// once, only when the fast-path misses and the pre-flight gate passes.
pub type ChatCallback = Box<dyn FnOnce(ChatRequest) -> ChatFuture + Send>;

pub enum CompressOutput {
    /// Pre-flight gate fired (non-system count ≤ keep_recent); even
    /// the truncate fallback couldn't shrink. Surfaces as
    /// `CompressionOutcome::StrategyDeclined`.
    NoOp,
    /// `summarized: true` means the output contains a summary
    /// message in place of the historical tool_use trail (LLM summary
    /// or fast-path); `false` means the tail was preserved verbatim
    /// (truncate fallback). The flag gates whether `ContextManager`
    /// re-attaches the skill trailer.
    Replaced {
        messages: Vec<ChatMessage>,
        summarized: bool,
    },
}

/// Trailing user prompt appended to the full conversation handed to
/// the summarizer LLM. The instruction forces a tool-free response
/// shaped as `<analysis>...</analysis><summary>...</summary>`; we
/// keep the `<summary>` block verbatim (tags included) and discard
/// the analysis.
///
/// Shared with `aura_agent::background_compression::build_summary_prompt`,
/// which prepends a prior-summary preamble and appends a SIZE TARGET
/// footer. Editing the analysis/summary contract here must stay
/// compatible with both call sites.
pub const SUMMARIZE_INSTRUCTION: &str = r#"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.

- Do NOT use Read, Bash, Grep, Glob, Edit, Write, or ANY other tool.
- You already have all the context you need in the conversation above.
- Tool calls will be REJECTED and will waste your only turn — you will fail the task.
- Your entire response must be plain text: an <analysis> block followed by a <summary> block.

Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions.
This summary should be thorough in capturing technical details, code patterns, and architectural decisions that would be essential for continuing development work without losing context.

Before providing your final summary, wrap your analysis in <analysis> tags to organize your thoughts and ensure you've covered all necessary points. In your analysis process:

1. Chronologically analyze each message and section of the conversation. For each section thoroughly identify:
   - The user's explicit requests and intents
   - Your approach to addressing the user's requests
   - Key decisions, technical concepts and code patterns
   - Specific details like:
     - file names
     - full code snippets
     - function signatures
     - file edits
   - Errors that you ran into and how you fixed them
   - Pay special attention to specific user feedback that you received, especially if the user told you to do something differently.
2. Double-check for technical accuracy and completeness, addressing each required element thoroughly.

Your summary should include the following sections:

1. Primary Request and Intent: Capture all of the user's explicit requests and intents in detail
2. Key Technical Concepts: List all important technical concepts, technologies, and frameworks discussed.
3. Files and Code Sections: Enumerate specific files and code sections examined, modified, or created. Pay special attention to the most recent messages and include full code snippets where applicable and include a summary of why this file read or edit is important.
4. Errors and fixes: List all errors that you ran into, and how you fixed them. Pay special attention to specific user feedback that you received, especially if the user told you to do something differently.
5. Problem Solving: Document problems solved and any ongoing troubleshooting efforts.
6. All user messages: List ALL user messages that are not tool results. These are critical for understanding the users' feedback and changing intent.
7. Pending Tasks: Outline any pending tasks that you have explicitly been asked to work on.
8. Current Work: Describe in detail precisely what was being worked on immediately before this summary request, paying special attention to the most recent messages from both user and assistant. Include file names and code snippets where applicable.
9. Optional Next Step: List the next step that you will take that is related to the most recent work you were doing. IMPORTANT: ensure that this step is DIRECTLY in line with the user's most recent explicit requests, and the task you were working on immediately before this summary request. If your last task was concluded, then only list next steps if they are explicitly in line with the users request. Do not start on tangential requests or really old requests that were already completed without confirming with the user first.
                       If there is a next step, include direct quotes from the most recent conversation showing exactly what task you were working on and where you left off. This should be verbatim to ensure there's no drift in task interpretation.

Here's an example of how your output should be structured:

<example>
<analysis>
[Your thought process, ensuring all points are covered thoroughly and accurately]
</analysis>

<summary>
1. Primary Request and Intent:
   [Detailed description]

2. Key Technical Concepts:
   - [Concept 1]
   - [Concept 2]
   - [...]

3. Files and Code Sections:
   - [File Name 1]
      - [Summary of why this file is important]
      - [Summary of the changes made to this file, if any]
      - [Important Code Snippet]
   - [File Name 2]
      - [Important Code Snippet]
   - [...]

4. Errors and fixes:
    - [Detailed description of error 1]:
      - [How you fixed the error]
      - [User feedback on the error if any]
    - [...]

5. Problem Solving:
   [Description of solved problems and ongoing troubleshooting]

6. All user messages:
    - [Detailed non tool use user message]
    - [...]

7. Pending Tasks:
   - [Task 1]
   - [Task 2]
   - [...]

8. Current Work:
   [Precise description of current work]

9. Optional Next Step:
   [Optional Next step to take]

</summary>
</example>

REMINDER: Do NOT call any tools. Respond with plain text only — an <analysis> block followed by a <summary> block. Tool calls will be rejected and you will fail the task."#;

fn find_tagged_block(text: &str, tag: &str) -> Option<std::ops::Range<usize>> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)?;
    let after_open = &text[start + open.len()..];
    let close_offset = after_open.find(&close)?;
    let end = start + open.len() + close_offset + close.len();
    Some(start..end)
}

fn strip_analysis_block(text: &str) -> String {
    match find_tagged_block(text, "analysis") {
        Some(range) => {
            let mut out = String::with_capacity(text.len() - (range.end - range.start));
            out.push_str(&text[..range.start]);
            out.push_str(&text[range.end..]);
            out
        }
        None => text.to_string(),
    }
}

/// Strip the `<analysis>` block, then return the `<summary>` block
/// verbatim if present, else the non-empty leftover. `None` only when
/// the leftover is empty / whitespace.
///
/// Shared with `aura_agent::background_compression` — both call sites
/// must agree on the tag contract or summaries silently corrupt when
/// one path's prompt is updated without the other's.
pub fn parse_summary_response(text: &str) -> Option<String> {
    let stripped = strip_analysis_block(text);
    if let Some(range) = find_tagged_block(&stripped, "summary") {
        let block = &stripped[range];
        if !block.trim().is_empty() {
            return Some(block.to_string());
        }
    }
    let leftover = stripped.trim();
    if leftover.is_empty() {
        None
    } else {
        Some(leftover.to_string())
    }
}

pub(crate) fn partition_system(messages: &[ChatMessage]) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
    let mut system = Vec::new();
    let mut rest = Vec::new();
    for msg in messages {
        if msg.role == aura_model::Role::System {
            system.push(msg.clone());
        } else {
            rest.push(msg.clone());
        }
    }
    (system, rest)
}

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
/// truncation paths that split on a fixed `keep_recent` boundary
/// must call this before slicing.
pub(crate) fn pair_preserving_cut(messages: &[ChatMessage], cut: usize) -> usize {
    let mut new_cut = cut.min(messages.len());
    if new_cut == 0 {
        return 0;
    }

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
            return new_cut;
        }
    }
}

impl ContextManager {
    pub(crate) async fn run_compression_flow(
        &self,
        chat: ChatCallback,
    ) -> crate::Result<CompressOutput> {
        if let Some(out) = self.try_summary_fast_path().await {
            return Ok(out);
        }

        // Skip the LLM call when even the truncate fallback couldn't
        // shrink — a /compact on a tiny conversation shouldn't burn
        // tokens producing a single-line summary.
        let (_, non_system) = partition_system(&self.messages);
        if non_system.len() <= self.keep_recent {
            return Ok(CompressOutput::NoOp);
        }

        Ok(self.summarize_or_truncate(chat).await)
    }

    /// Stage 1: try to assemble `[system + summary.md + recent slice]`.
    /// Returns `None` on any fall-through condition; all such
    /// conditions log at debug/warn so production has a paper trail.
    async fn try_summary_fast_path(&self) -> Option<CompressOutput> {
        // Wait for any in-flight background pass to land first so we
        // pick up the fresher cursor instead of re-summarizing content
        // it already covered. Bounded — a stuck refresh can't block a
        // user turn indefinitely; on timeout we proceed with whatever
        // metadata is on file (stale-by-one tolerated).
        let wait_future = async {
            loop {
                match self.sessions.summary_metadata(&self.session_id).await {
                    Ok(Some(meta)) if meta.in_flight => {
                        tokio::time::sleep(SUMMARY_REFRESH_WAIT_POLL_INTERVAL).await;
                    }
                    _ => return,
                }
            }
        };
        if tokio::time::timeout(SUMMARY_REFRESH_WAIT_TIMEOUT, wait_future)
            .await
            .is_err()
        {
            warn!(
                session_id = %self.session_id,
                "fast-path: in-flight refresh did not settle within timeout; \
                 proceeding with last recorded metadata"
            );
        }

        let metadata = match self.sessions.summary_metadata(&self.session_id).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                debug!(
                    session_id = %self.session_id,
                    "fast-path: no summary metadata; falling through to LLM summary"
                );
                return None;
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "fast-path: summary metadata read failed; falling through"
                );
                return None;
            }
        };
        let summary_content = match self.summary_loader.load(&self.session_id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                warn!(
                    session_id = %self.session_id,
                    cursor = metadata.cursor,
                    "fast-path: metadata exists but summary.md missing; falling through"
                );
                return None;
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "fast-path: summary.md read failed; falling through"
                );
                return None;
            }
        };

        // Map `metadata.cursor` (a `session_messages.ordinal`) back to
        // an in-memory index so the recent slice can't be cut past
        // the cursor and silently drop unsummarized middle history.
        // Any mismatch with `messages.len()` collapses the fast-path
        // — we can't prove cursor coverage, so fall through.
        let cursor_idx_in_active = match self
            .sessions
            .active_index_of_ordinal(&self.session_id, metadata.cursor)
            .await
        {
            Ok(Some(idx)) => idx,
            Ok(None) => {
                debug!(
                    session_id = %self.session_id,
                    cursor = metadata.cursor,
                    "fast-path: cursor ordinal not present in active log; falling through"
                );
                return None;
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "fast-path: cursor index lookup failed; falling through"
                );
                return None;
            }
        };
        match self.sessions.count_active_messages(&self.session_id).await {
            Ok(active_count) if active_count == self.messages.len() => {}
            Ok(active_count) => {
                debug!(
                    session_id = %self.session_id,
                    active_count,
                    in_memory_len = self.messages.len(),
                    "fast-path: active log / in-memory length mismatch; falling through"
                );
                return None;
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "fast-path: active count lookup failed; falling through"
                );
                return None;
            }
        }

        let (system_msgs, non_system) = partition_system(&self.messages);

        // A cursor inside the system block is degenerate — the
        // summary would cover only soul-prompt content and we can't
        // reason about the post-cursor tail.
        let cursor_idx_in_non_system = if cursor_idx_in_active >= system_msgs.len() {
            cursor_idx_in_active - system_msgs.len()
        } else {
            debug!(
                session_id = %self.session_id,
                cursor_idx_in_active,
                system_count = system_msgs.len(),
                "fast-path: cursor falls inside the system block; falling through"
            );
            return None;
        };

        // The recent slice must cover every message *strictly* after
        // the cursor (those are unsummarized originals); it may
        // extend further back when the natural walk needs more to
        // satisfy `RECENT_SLICE_MIN_*`. `RECENT_SLICE_MAX_TOKENS` is
        // a forward-extension ceiling — it never trims unsummarized
        // content. `pair_preserving_cut` guards against a cursor
        // landing between `assistant{tool_use}` and the matching
        // `user{tool_result}`, which both Anthropic and OpenAI reject.
        let tokenize_msg = |m: &ChatMessage| self.tokenizer.count_message(m);
        let walk_cut = walk_backward_atomic(
            &non_system,
            RECENT_SLICE_MIN_TOKENS,
            RECENT_SLICE_MIN_TEXT_BLOCK_MSGS,
            RECENT_SLICE_MAX_TOKENS,
            tokenize_msg,
        );
        let post_cursor_cut = (cursor_idx_in_non_system + 1).min(non_system.len());
        let cut = pair_preserving_cut(&non_system, walk_cut.min(post_cursor_cut));
        let recent_slice = non_system[cut..].to_vec();

        let summary_msg = build_summary_message(&summary_content);
        let summary_tokens = self.tokenizer.count_message(&summary_msg);
        let called = scan_skill_calls(&recent_slice);
        let skill_trailer_tokens = estimate_skill_trailer_tokens(
            self.skill_registry.as_ref(),
            self.tokenizer.as_ref(),
            &called,
        );
        let recent_slice_tokens: usize = recent_slice
            .iter()
            .map(|m| self.tokenizer.count_message(m))
            .sum();
        let fallthrough_budget =
            (self.budget.max_tokens() as f64 * FAST_PATH_FALLTHROUGH_THRESHOLD_RATIO) as usize;
        // Recent slice counts toward the budget: a far-back cursor
        // can leave post-cursor content larger than summary + trailer,
        // and an over-budget assembly would just re-trip on the next
        // turn.
        let assembled_tokens = summary_tokens + skill_trailer_tokens + recent_slice_tokens;
        if assembled_tokens > fallthrough_budget {
            warn!(
                session_id = %self.session_id,
                summary_tokens,
                skill_trailer_tokens,
                recent_slice_tokens,
                fallthrough_budget,
                "fast-path: assembled total exceeds fall-through threshold; falling through"
            );
            return None;
        }

        let mut new_messages = system_msgs;
        new_messages.push(summary_msg);
        new_messages.extend(recent_slice);

        debug!(
            session_id = %self.session_id,
            cursor = metadata.cursor,
            cursor_idx_in_non_system,
            cut,
            recent_msg_count = (non_system.len() - cut),
            summary_tokens,
            recent_slice_tokens,
            skill_trailer_tokens,
            "fast-path: assembled list with precomputed summary"
        );

        Some(CompressOutput::Replaced {
            messages: new_messages,
            summarized: true,
        })
    }

    /// Stages 2 + 3. Always returns `Replaced` — the pre-flight gate
    /// already filtered the "nothing to shrink" case, and the
    /// truncate fallback is guaranteed to shorten when reached.
    /// `summarized` is `true` on LLM-summary success, `false` on the
    /// truncate fallback.
    async fn summarize_or_truncate(&self, chat: ChatCallback) -> CompressOutput {
        let (system_msgs, non_system) = partition_system(&self.messages);

        let mut request_messages: Vec<ChatMessage> = self.messages.to_vec();
        request_messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(SUMMARIZE_INSTRUCTION.to_string())],
        });
        let request = ChatRequest {
            messages: request_messages,
            temperature: None,
            tools: Vec::new(),
        };

        let truncate_fallback = || {
            let mut out = system_msgs.clone();
            let initial_split = non_system.len().saturating_sub(self.keep_recent);
            let split = pair_preserving_cut(&non_system, initial_split);
            out.extend_from_slice(&non_system[split..]);
            CompressOutput::Replaced {
                messages: out,
                summarized: false,
            }
        };

        match chat(request).await {
            Ok(response) => match parse_summary_response(&response.content) {
                Some(content) => {
                    let mut new_messages = system_msgs;
                    new_messages.push(ChatMessage {
                        role: Role::User,
                        content: vec![ContentBlock::Text(content)],
                    });
                    CompressOutput::Replaced {
                        messages: new_messages,
                        summarized: true,
                    }
                }
                None => {
                    warn!(
                        "summarizer response empty after stripping analysis; falling back to truncation"
                    );
                    truncate_fallback()
                }
            },
            Err(e) => {
                warn!(error = %e, "summarization failed; falling back to truncation");
                truncate_fallback()
            }
        }
    }
}

fn build_summary_message(summary_content: &str) -> ChatMessage {
    let body = format!(
        "<context-summary>\n{}\n\n{}\n</context-summary>",
        CONTEXT_SUMMARY_WRAPPER_PREAMBLE,
        summary_content.trim()
    );
    ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::Text(body)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let msgs = vec![
            text(Role::User, "ask"),
            tool_use("tu1"),
            tool_result("tu1"),
            text(Role::Assistant, "done"),
        ];
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
        assert_eq!(pair_preserving_cut(&msgs, 2), 2);
    }

    #[test]
    fn dangling_tool_result_pulls_back_through_intermediate_messages() {
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
        assert_eq!(pair_preserving_cut(&msgs, 3), 0);
    }

    #[test]
    fn cut_zero_or_full_is_noop() {
        let msgs = vec![tool_use("tu1"), tool_result("tu1")];
        assert_eq!(pair_preserving_cut(&msgs, 0), 0);
        assert_eq!(pair_preserving_cut(&msgs, 2), 2);
        assert_eq!(pair_preserving_cut(&msgs, 99), 2);
    }

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
        let msgs: Vec<ChatMessage> = (0..6).map(|i| text(Role::User, &format!("m{i}"))).collect();
        let cut = walk_backward_atomic(&msgs, 25, 3, 100, flat_tokenize);
        assert_eq!(cut, 3);
    }

    #[test]
    fn walk_keeps_walking_when_only_text_blocks_min_satisfied_but_not_tokens() {
        let msgs: Vec<ChatMessage> = (0..6).map(|i| text(Role::User, &format!("m{i}"))).collect();
        let cut = walk_backward_atomic(&msgs, 50, 1, 100, flat_tokenize);
        assert_eq!(cut, 1);
    }

    #[test]
    fn walk_hard_cap_drops_unit_that_overflows() {
        let msgs: Vec<ChatMessage> = (0..5).map(|i| text(Role::User, &format!("m{i}"))).collect();
        let cut = walk_backward_atomic(&msgs, 1000, 1000, 25, flat_tokenize);
        assert_eq!(cut, 3);
    }

    #[test]
    fn walk_returns_full_length_when_first_unit_exceeds_max() {
        let msgs = vec![text(Role::User, "big")];
        let cut = walk_backward_atomic(&msgs, 1, 1, 5, flat_tokenize);
        assert_eq!(cut, 1);
    }

    #[test]
    fn walk_treats_tool_use_pair_atomically() {
        let msgs = vec![text(Role::User, "ask"), tool_use("tu1"), tool_result("tu1")];
        let cut = walk_backward_atomic(&msgs, 1, 0, 100, flat_tokenize);
        assert_eq!(cut, 1);
    }

    #[test]
    fn walk_drops_atomic_pair_that_exceeds_cap() {
        let msgs = vec![text(Role::User, "ask"), tool_use("tu1"), tool_result("tu1")];
        let cut = walk_backward_atomic(&msgs, 1, 0, 5, flat_tokenize);
        assert_eq!(cut, 3);
    }

    #[test]
    fn walk_takes_everything_when_minima_unreachable() {
        let msgs: Vec<ChatMessage> = (0..3).map(|i| text(Role::User, &format!("m{i}"))).collect();
        let cut = walk_backward_atomic(&msgs, 100, 100, 1_000, flat_tokenize);
        assert_eq!(cut, 0);
    }

    #[test]
    fn parse_summary_response_picks_first_summary_after_stripping_analysis() {
        let text =
            "<analysis>a</analysis><summary>FIRST</summary> trailing <summary>SECOND</summary>";
        assert_eq!(
            parse_summary_response(text).as_deref(),
            Some("<summary>FIRST</summary>")
        );
    }

    #[test]
    fn parse_summary_response_returns_leftover_when_summary_missing() {
        let text = "<analysis>thinking</analysis>\n\nplain leftover body";
        assert_eq!(
            parse_summary_response(text).as_deref(),
            Some("plain leftover body")
        );
    }

    #[test]
    fn parse_summary_response_handles_no_analysis_block() {
        let text = "<summary>S</summary>";
        assert_eq!(
            parse_summary_response(text).as_deref(),
            Some("<summary>S</summary>")
        );

        let text = "no tags whatsoever";
        assert_eq!(
            parse_summary_response(text).as_deref(),
            Some("no tags whatsoever")
        );
    }

    #[test]
    fn parse_summary_response_returns_none_when_empty_after_strip() {
        assert!(parse_summary_response("").is_none());
        assert!(parse_summary_response("   \n  ").is_none());
        assert!(parse_summary_response("<analysis>x</analysis>").is_none());
        assert!(parse_summary_response("<analysis>x</analysis>   \n  ").is_none());
    }

    #[test]
    fn strip_analysis_block_is_noop_when_absent() {
        assert_eq!(strip_analysis_block("nothing here"), "nothing here");
        assert_eq!(
            strip_analysis_block("<analysis>no close"),
            "<analysis>no close"
        );
    }
}
