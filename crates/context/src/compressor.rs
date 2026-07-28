//! The context compaction flow.
//!
//! One summarizer LLM call replaces the conversation so far with
//! `[system…, summary, verbatim recent slice]`. The slice is bounded by
//! [`recent_slice_bounds`] and dropped when it would keep the result from
//! shrinking; on a summarizer failure or an unusable response the
//! transcript is truncated to `system + last keep_recent non-system`
//! instead (pair-preserving, so tool_use / tool_result stays intact).
//!
//! Two things short-circuit before any of that: a conversation already at
//! or below `keep_recent` non-system messages returns
//! [`CompressOutput::NoOp`] (even truncation couldn't shrink it), and a
//! cancelled summarizer call returns [`CompressOutput::Cancelled`], leaving
//! the transcript exactly as it was.
//!
//! See `docs/modules/context.md`.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use baybo_llm::{ChatRequest, LlmResponse};
use baybo_model::{ChatMessage, ContentBlock};
use baybo_trace::LlmCallInputs;
use tracing::{debug, warn};

use crate::error::ContextError;
use crate::prompts::compression::{
    CONTINUATION_FOOTER, CONTINUATION_FOOTER_WITH_SLICE, CONTINUATION_INTRO, SUMMARIZE_INSTRUCTION,
};
use crate::{
    ContextManager, MIN_COMPACTABLE_TOKENS, estimate_skill_trailer_tokens, recent_slice_bounds,
};

pub type ChatFuture =
    Pin<Box<dyn Future<Output = std::result::Result<LlmResponse, ContextError>> + Send>>;

/// One-shot chat invocation handed to the compressor. Invoked at most
/// once, only when the pre-flight gate passes.
/// The second argument is the trace `input_messages` marker the LLM
/// span should record — a `Persisted` ordinal reference (so the large
/// transcript prefix isn't cloned into the span) or an inline fallback;
/// the runtime stamps it onto the span rather than re-deriving it from
/// `ChatRequest.messages`.
pub type ChatCallback = Box<dyn FnOnce(ChatRequest, LlmCallInputs) -> ChatFuture + Send>;

pub enum CompressOutput {
    /// Pre-flight gate fired (non-system count ≤ keep_recent); even
    /// the truncate fallback couldn't shrink. Surfaces as
    /// `CompressionOutcome::StrategyDeclined`.
    NoOp,
    /// The summariser call was aborted by a turn cancellation. Nothing is
    /// applied; the transcript is still over budget, so the next turn's
    /// threshold check runs the compaction again.
    Cancelled,
    /// Compressor produced a new transcript. `ContextManager` always
    /// re-attaches the skill trailer here, since every Replaced
    /// branch can drop the historical `<system-reminder>` carrying
    /// the skill list — summary stages by construction, and the
    /// truncate fallback whenever the reminder lands in the dropped
    /// middle.
    Replaced {
        messages: Vec<ChatMessage>,
        /// Which path produced this replacement.
        stage: CompressionStage,
    },
}

/// How the transcript was actually shrunk. `Truncate` runs no LLM call, so it
/// leaves no `LlmCall` span and has to be recorded by the caller to be visible
/// in a trace at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionStage {
    /// Summarised the transcript with a live LLM call.
    LiveSummary,
    /// Dropped the middle of the transcript after the summarizer failed.
    Truncate,
}

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

/// Strip the `<analysis>` block, then return the inner body of the
/// `<summary>` block (tags removed) if present, else the non-empty
/// leftover. `None` only when nothing usable remains.
///
/// Returning the inner body rather than the wrapped block lets the compressor
/// put its own `Summary:` prefix on it, and keeps the tags out of the
/// transcript the model reads back.
pub fn parse_summary_response(text: &str) -> Option<String> {
    let stripped = strip_analysis_block(text);
    if let Some(inner) = find_tagged_inner(&stripped, "summary")
        && !inner.trim().is_empty()
    {
        return Some(inner.trim().to_string());
    }
    let leftover = stripped.trim();
    if leftover.is_empty() {
        None
    } else {
        Some(leftover.to_string())
    }
}

/// Like [`find_tagged_block`] but returns the inner body (between the
/// open and close tags), not the wrapped block.
fn find_tagged_inner<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let close_offset = text[start..].find(&close)?;
    Some(&text[start..start + close_offset])
}

pub(crate) fn partition_system(messages: &[ChatMessage]) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
    let mut system = Vec::new();
    let mut rest = Vec::new();
    for msg in messages {
        if msg.role == baybo_model::Role::System {
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
        // Decline only when there is genuinely nothing to gain: too few
        // messages for truncation to drop any, AND too little text for a
        // summary to beat its own framing. Both, not either.
        //
        // The message count alone is the truncate fallback's question — it
        // keeps the last `keep_recent`, so at or below that it can't shrink.
        // It says nothing about the summariser, which collapses any number of
        // messages into one. Gating on it alone refused to compact a
        // transcript of a few pasted files: ten messages, 26k tokens, well
        // past the budget, declined four turns running.
        let (_, non_system) = partition_system(&self.messages);
        let non_system_tokens: usize = non_system
            .iter()
            .map(|m| self.message_budget_tokens(m).total())
            .sum();
        if non_system.len() <= self.keep_recent && non_system_tokens <= MIN_COMPACTABLE_TOKENS {
            return Ok(CompressOutput::NoOp);
        }

        Ok(self.summarize_or_truncate(chat).await)
    }

    /// The compaction itself: one summarizer call, then assemble.
    ///
    /// Returns `Replaced` in every case but a cancellation — the pre-flight
    /// gate already filtered "nothing to shrink", and the truncate fallback is
    /// guaranteed to shorten when reached.
    async fn summarize_or_truncate(&self, chat: ChatCallback) -> CompressOutput {
        let (system_msgs, non_system) = partition_system(&self.messages);

        let instruction =
            ChatMessage::agent_context(vec![ContentBlock::Text(SUMMARIZE_INSTRUCTION.to_string())]);
        let mut request_messages: Vec<ChatMessage> = self.messages.to_vec();
        request_messages.push(instruction.clone());

        // Reference the (large) transcript prefix by ordinal in the trace
        // when the in-memory set provably mirrors the persisted log;
        // `instruction` is the only message not in `session_messages`, so
        // it rides as the suffix. On any mismatch fall back to inline.
        //
        // This is also why the whole transcript is sent even though the tail
        // is about to be kept verbatim: `Persisted` can only name the entire
        // active set, so trimming the request to a strict prefix would force
        // an `Inline` marker and re-embed the transcript into every
        // compaction span.
        let input_marker = match self.synced_last_ordinal().await {
            Some((last_ordinal, prefix_len)) => LlmCallInputs::Persisted {
                last_ordinal,
                prefix_len,
                suffix: vec![instruction],
            },
            None => LlmCallInputs::Inline(request_messages.clone()),
        };
        let request = ChatRequest {
            messages: request_messages,
            temperature: None,
            tools: Vec::new(),
            reasoning_effort: None,
        };

        let truncate_fallback = || {
            let mut out = system_msgs.clone();
            let initial_split = non_system.len().saturating_sub(self.keep_recent);
            let split = pair_preserving_cut(&non_system, initial_split);
            out.extend_from_slice(&non_system[split..]);
            CompressOutput::Replaced {
                messages: out,
                stage: CompressionStage::Truncate,
            }
        };

        let summary = match chat(request, input_marker).await {
            Ok(response) => parse_summary_response(&response.content),
            // Not a failure to summarise — the call was cut short, so nothing
            // was learned about the transcript. Truncating on that would
            // destroy the middle of the conversation over a `/stop`.
            Err(ContextError::Cancelled(reason)) => {
                debug!(%reason, "compaction cancelled; leaving it for the next turn");
                return CompressOutput::Cancelled;
            }
            Err(e) => {
                warn!(error = %e, "summarization failed; falling back to truncation");
                None
            }
        };
        let Some(summary) = summary else {
            return truncate_fallback();
        };

        self.assemble_summary(system_msgs, &non_system, &summary)
    }

    /// Assemble `[system…, summary, recent slice…]`, or `[system…, summary]`
    /// when the slice doesn't pay for itself.
    ///
    /// The slice is what keeps a compaction from turning the last tool
    /// results and the user's own words into a paraphrase of themselves. But
    /// it is also re-added to the compacted transcript, so on a short
    /// conversation — a `/compact` typed early, a small context window — the
    /// walk can pull in nearly everything and the "compacted" result comes
    /// out no smaller than what it replaced. Rather than spend the
    /// summarizer call and then decline to apply it, pick between the two
    /// assemblies here: the summary is already in hand, so this costs a
    /// tokenize, not a round-trip.
    fn assemble_summary(
        &self,
        system_msgs: Vec<ChatMessage>,
        non_system: &[ChatMessage],
        summary: &str,
    ) -> CompressOutput {
        let transcript_path = self.workspace.session_log_file(self.session_id.as_str());

        let (min_tokens, min_text_block_msgs, max_tokens) =
            recent_slice_bounds(self.budget.max_tokens());
        let cut = walk_backward_atomic(
            non_system,
            min_tokens,
            min_text_block_msgs,
            max_tokens,
            |m| self.message_budget_tokens(m).total(),
        );

        let with_slice = || {
            let mut out = system_msgs.clone();
            out.push(build_summary_message(summary, &transcript_path, true));
            out.extend_from_slice(&non_system[cut..]);
            out
        };
        let summary_only = || {
            let mut out = system_msgs.clone();
            out.push(build_summary_message(summary, &transcript_path, false));
            out
        };

        for candidate in [with_slice(), summary_only()] {
            if self.compaction_fits(&candidate) {
                return CompressOutput::Replaced {
                    messages: candidate,
                    stage: CompressionStage::LiveSummary,
                };
            }
        }
        // Neither shrinks. Hand back the richer one and let
        // `run_compression`'s savings gate reject it — one place decides
        // whether an apply is worth it.
        CompressOutput::Replaced {
            messages: with_slice(),
            stage: CompressionStage::LiveSummary,
        }
    }

    /// Whether a candidate assembly is small enough to be worth applying:
    /// strictly smaller than what it replaces, and under the ceiling whose
    /// crossing triggers the next compaction — otherwise it would compact
    /// again on the very next iteration.
    ///
    /// Counted the way [`ContextManager::run_compression`]'s savings gate
    /// counts, trailer included, so a candidate accepted here can't be
    /// rejected there.
    fn compaction_fits(&self, candidate: &[ChatMessage]) -> bool {
        let body: usize = candidate
            .iter()
            .map(|m| self.message_budget_tokens(m).total())
            .sum();
        let trailer = estimate_skill_trailer_tokens(
            self.skill_registry.as_ref(),
            self.tokenizer.as_ref(),
            &self.called_skills,
            &self.invocable_skill_summaries(),
        );
        let total = self.calibrate(body) + trailer;
        total < self.budget.current() && total <= self.budget.compression_ceiling()
    }
}

/// Build a continuation-style summary message from the parsed summary body.
/// `transcript_path` points at the per-session JSONL the agent loop appends
/// to, so the model can go read specific pre-compaction details.
///
/// `with_slice` tells the model whether verbatim recent messages actually
/// follow — the footer must not promise a tail the assembly then dropped.
fn build_summary_message(
    body: &str,
    transcript_path: &std::path::Path,
    with_slice: bool,
) -> ChatMessage {
    let body_block = format!("Summary:\n{}", body.trim());
    let slice_note = if with_slice {
        CONTINUATION_FOOTER_WITH_SLICE
    } else {
        ""
    };
    let text = format!(
        "{intro}\n\n{body}\n\nIf you need specific details from before compaction (like exact code snippets, error messages, or content you generated), read the full transcript at: {transcript}\n\n{slice_note}{footer}",
        intro = CONTINUATION_INTRO,
        body = body_block,
        transcript = transcript_path.display(),
        slice_note = slice_note,
        footer = CONTINUATION_FOOTER,
    );
    ChatMessage::agent_context(vec![ContentBlock::Text(text)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::Role;

    fn tool_use(id: &str) -> ChatMessage {
        ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: id.into(),
            name: "bash".into(),
            input: serde_json::Value::Null,
            signature: None,
        }])
    }

    fn tool_result(id: &str) -> ChatMessage {
        ChatMessage::agent_context(vec![ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: "ok".into(),
            meta: None,
        }])
    }

    fn text(role: Role, t: &str) -> ChatMessage {
        let content = vec![ContentBlock::Text(t.into())];
        match role {
            Role::User => ChatMessage::agent_context(content),
            Role::Assistant => ChatMessage::assistant(content),
            Role::System => ChatMessage::system(content),
            Role::Tool => ChatMessage::tool(content),
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
        assert_eq!(parse_summary_response(text).as_deref(), Some("FIRST"));
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
        assert_eq!(parse_summary_response(text).as_deref(), Some("S"));

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
