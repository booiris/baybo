//! Inline `<think>...</think>` tag handling for providers that emit
//! chain-of-thought reasoning embedded in the regular text channel
//! (MiniMax M2 today; possibly others in the future). rig's OpenAI
//! adapter only recognizes the separate `reasoning_content` field —
//! anything inside `<think>` tags reaches us as plain assistant text.
//!
//! This module strips those tags and re-routes the inner content to
//! the `thinking` field so downstream consumers (UI, trace, history
//! round-trip) see the same shape as native reasoning providers.

use crate::StreamEvent;

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// Strip inline `<think>...</think>` blocks from a complete text body.
/// Returns the cleaned text and the concatenation of all extracted
/// thinking segments (joined with newlines).
pub(crate) fn extract_inline_thinking(text: &str) -> (String, Option<String>) {
    let mut clean = String::new();
    let mut thinking = String::new();
    let mut rest = text;

    loop {
        match rest.find(OPEN) {
            None => {
                clean.push_str(rest);
                break;
            }
            Some(start) => {
                clean.push_str(&rest[..start]);
                let after_open = &rest[start + OPEN.len()..];
                match after_open.find(CLOSE) {
                    None => {
                        if !thinking.is_empty() {
                            thinking.push('\n');
                        }
                        thinking.push_str(after_open);
                        break;
                    }
                    Some(end) => {
                        if !thinking.is_empty() {
                            thinking.push('\n');
                        }
                        thinking.push_str(&after_open[..end]);
                        rest = &after_open[end + CLOSE.len()..];
                    }
                }
            }
        }
    }

    let extracted = if thinking.is_empty() {
        None
    } else {
        Some(thinking)
    };
    (clean, extracted)
}

/// Streaming filter that consumes incremental text chunks and yields
/// `StreamEvent`s, splitting `<think>...</think>` content out into
/// `Reasoning` deltas. Tags split across chunk boundaries are buffered
/// until they can be fully resolved.
#[derive(Default)]
pub(crate) struct ThinkTagFilter {
    inside: bool,
    /// Trailing characters that could be the start of an open/close tag,
    /// held back across chunks.
    pending: String,
    /// All reasoning text seen so far. Flushed as a single
    /// `ThinkingBlock` at end-of-stream so history round-trip works.
    accumulated: String,
}

impl ThinkTagFilter {
    pub(crate) fn push_text(&mut self, chunk: &str) -> Vec<StreamEvent> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(chunk);
        let mut events = Vec::new();

        loop {
            let target = if self.inside { CLOSE } else { OPEN };
            if let Some(idx) = buf.find(target) {
                if idx > 0 {
                    let before = buf[..idx].to_string();
                    self.emit(&before, &mut events);
                }
                buf.drain(..idx + target.len());
                self.inside = !self.inside;
                continue;
            }

            let hold = trailing_partial_len(&buf, target);
            if hold == buf.len() {
                self.pending = buf;
                return events;
            }
            let split = buf.len() - hold;
            if split > 0 {
                let emit_part = buf[..split].to_string();
                self.emit(&emit_part, &mut events);
            }
            self.pending = buf[split..].to_string();
            return events;
        }
    }

    pub(crate) fn flush(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let leftover = std::mem::take(&mut self.pending);
        if !leftover.is_empty() {
            self.emit(&leftover, &mut events);
        }
        if !self.accumulated.is_empty() {
            let text = std::mem::take(&mut self.accumulated);
            events.push(StreamEvent::ThinkingBlock(
                aura_model::ContentBlock::Thinking {
                    id: None,
                    content: vec![aura_model::ThinkingContent::Text {
                        text,
                        signature: None,
                    }],
                },
            ));
        }
        events
    }

    fn emit(&mut self, text: &str, events: &mut Vec<StreamEvent>) {
        if self.inside {
            self.accumulated.push_str(text);
            events.push(StreamEvent::Reasoning(text.to_string()));
        } else {
            events.push(StreamEvent::Text(text.to_string()));
        }
    }
}

/// Largest `n` such that the trailing `n` bytes of `buf` are a non-empty
/// proper prefix of `target` and lie on a UTF-8 char boundary. Used to
/// hold back potential split-across-chunks tag starts.
fn trailing_partial_len(buf: &str, target: &str) -> usize {
    let max = (target.len() - 1).min(buf.len());
    for i in (1..=max).rev() {
        let split = buf.len() - i;
        if !buf.is_char_boundary(split) {
            continue;
        }
        if target.starts_with(&buf[split..]) {
            return i;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ContentBlock, ThinkingContent};

    #[test]
    fn extract_no_tags_passes_through() {
        let (clean, thinking) = extract_inline_thinking("hello world");
        assert_eq!(clean, "hello world");
        assert!(thinking.is_none());
    }

    #[test]
    fn extract_single_block() {
        let (clean, thinking) =
            extract_inline_thinking("<think>step 1\nstep 2</think>final answer");
        assert_eq!(clean, "final answer");
        assert_eq!(thinking.as_deref(), Some("step 1\nstep 2"));
    }

    #[test]
    fn extract_block_with_surrounding_text() {
        let (clean, thinking) = extract_inline_thinking("prefix <think>secret</think> suffix");
        assert_eq!(clean, "prefix  suffix");
        assert_eq!(thinking.as_deref(), Some("secret"));
    }

    #[test]
    fn extract_multiple_blocks_join_with_newline() {
        let (clean, thinking) = extract_inline_thinking("a<think>x</think>b<think>y</think>c");
        assert_eq!(clean, "abc");
        assert_eq!(thinking.as_deref(), Some("x\ny"));
    }

    #[test]
    fn extract_unclosed_tag_consumes_remainder() {
        let (clean, thinking) = extract_inline_thinking("answer<think>truncated reasoning");
        assert_eq!(clean, "answer");
        assert_eq!(thinking.as_deref(), Some("truncated reasoning"));
    }

    fn collect_text_and_reasoning(events: &[StreamEvent]) -> (String, String) {
        let mut text = String::new();
        let mut reasoning = String::new();
        for ev in events {
            match ev {
                StreamEvent::Text(t) => text.push_str(t),
                StreamEvent::Reasoning(r) => reasoning.push_str(r),
                _ => {}
            }
        }
        (text, reasoning)
    }

    fn final_thinking_block(events: &[StreamEvent]) -> Option<String> {
        events.iter().find_map(|ev| match ev {
            StreamEvent::ThinkingBlock(ContentBlock::Thinking { content, .. }) => {
                content.first().map(|c| match c {
                    ThinkingContent::Text { text, .. } => text.clone(),
                    ThinkingContent::Summary { text } => text.clone(),
                    ThinkingContent::Redacted { .. } => String::new(),
                })
            }
            _ => None,
        })
    }

    #[test]
    fn stream_no_tags() {
        let mut f = ThinkTagFilter::default();
        let mut events = f.push_text("hello world");
        events.extend(f.flush());
        let (text, reasoning) = collect_text_and_reasoning(&events);
        assert_eq!(text, "hello world");
        assert!(reasoning.is_empty());
        assert!(final_thinking_block(&events).is_none());
    }

    #[test]
    fn stream_single_tag_in_one_chunk() {
        let mut f = ThinkTagFilter::default();
        let mut events = f.push_text("hi <think>secret</think> bye");
        events.extend(f.flush());
        let (text, reasoning) = collect_text_and_reasoning(&events);
        assert_eq!(text, "hi  bye");
        assert_eq!(reasoning, "secret");
        assert_eq!(final_thinking_block(&events).as_deref(), Some("secret"));
    }

    #[test]
    fn stream_open_tag_split_across_chunks() {
        let mut f = ThinkTagFilter::default();
        let mut events = f.push_text("foo <th");
        events.extend(f.push_text("ink>secret</think> bar"));
        events.extend(f.flush());
        let (text, reasoning) = collect_text_and_reasoning(&events);
        assert_eq!(text, "foo  bar");
        assert_eq!(reasoning, "secret");
    }

    #[test]
    fn stream_close_tag_split_across_chunks() {
        let mut f = ThinkTagFilter::default();
        let mut events = f.push_text("<think>reasoning</thi");
        events.extend(f.push_text("nk>answer"));
        events.extend(f.flush());
        let (text, reasoning) = collect_text_and_reasoning(&events);
        assert_eq!(text, "answer");
        assert_eq!(reasoning, "reasoning");
    }

    #[test]
    fn stream_char_by_char_emits_correctly() {
        let mut f = ThinkTagFilter::default();
        let input = "ab<think>xy</think>cd";
        let mut events = Vec::new();
        for ch in input.chars() {
            events.extend(f.push_text(&ch.to_string()));
        }
        events.extend(f.flush());
        let (text, reasoning) = collect_text_and_reasoning(&events);
        assert_eq!(text, "abcd");
        assert_eq!(reasoning, "xy");
        assert_eq!(final_thinking_block(&events).as_deref(), Some("xy"));
    }

    #[test]
    fn stream_unclosed_tag_flushes_as_reasoning() {
        let mut f = ThinkTagFilter::default();
        let mut events = f.push_text("answer<think>incomplete");
        events.extend(f.flush());
        let (text, reasoning) = collect_text_and_reasoning(&events);
        assert_eq!(text, "answer");
        assert_eq!(reasoning, "incomplete");
    }

    #[test]
    fn stream_lone_lt_passes_through_on_flush() {
        let mut f = ThinkTagFilter::default();
        let mut events = f.push_text("a < b");
        events.extend(f.flush());
        let (text, reasoning) = collect_text_and_reasoning(&events);
        assert_eq!(text, "a < b");
        assert!(reasoning.is_empty());
    }

    #[test]
    fn stream_partial_lt_held_then_resolved_as_text() {
        let mut f = ThinkTagFilter::default();
        let mut events = f.push_text("hello<");
        events.extend(f.push_text("not-a-tag"));
        events.extend(f.flush());
        let (text, reasoning) = collect_text_and_reasoning(&events);
        assert_eq!(text, "hello<not-a-tag");
        assert!(reasoning.is_empty());
    }

    #[test]
    fn stream_multiple_blocks() {
        let mut f = ThinkTagFilter::default();
        let mut events = f.push_text("a<think>1</think>b<think>2</think>c");
        events.extend(f.flush());
        let (text, reasoning) = collect_text_and_reasoning(&events);
        assert_eq!(text, "abc");
        assert_eq!(reasoning, "12");
        assert_eq!(final_thinking_block(&events).as_deref(), Some("12"));
    }
}
