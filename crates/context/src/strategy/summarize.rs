use async_trait::async_trait;
use aura_llm::ChatRequest;
use aura_model::{ChatMessage, ContentBlock, Role};
use tracing::warn;

use super::{ChatCallback, CompressOutput, CompressionStrategy, pair_preserving_cut};

/// Trailing user prompt appended to the full conversation handed to
/// the summarizer LLM. The instruction forces a tool-free response
/// shaped as `<analysis>...</analysis><summary>...</summary>`; we
/// keep the `<summary>` block verbatim (tags included) and discard
/// the analysis.
const SUMMARIZE_INSTRUCTION: &str = r#"CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.

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

/// Summarize compression: hands the entire conversation (system +
/// non-system) plus a trailing summarisation prompt to the LLM, then
/// replaces the conversation with the original system messages plus a
/// single user message containing the parsed summary.
///
/// The strategy itself is a pure planner: it produces a summary
/// message and signals `replaced_full_history: true` so
/// `ContextManager` knows to attach the post-compression skill
/// trailer (authoritative reminder + per-skill detail blocks) on top
/// of the returned slice.
///
/// Parsing is two-stage: first the `<analysis>...</analysis>` block is
/// stripped, then a `<summary>...</summary>` block is extracted (tags
/// kept) from the remainder. If no `<summary>` block is present, the
/// leftover content (everything except the analysis block) is used
/// verbatim — that way a non-conforming LLM response still produces a
/// usable summary instead of paying for the truncation fallback.
///
/// `keep_recent` shapes the deterministic fallback used on transport /
/// empty-response failure: how many non-system messages to retain at
/// the tail when the summarizer can't produce a usable response. It
/// does **not** gate whether the strategy runs — `ContextManager`'s
/// budget threshold (`maybe_compress`) is the only "should we compress
/// now" check, and `/compact` (`force_compress`) deliberately bypasses
/// even that. A forced summarize on a too-small conversation that
/// can't actually shrink surfaces as `CompressionOutcome::NoSavings`
/// via the manager's post-compression token check.
pub struct Summarize {
    keep_recent: usize,
}

impl Summarize {
    pub fn new(keep_recent: usize) -> Self {
        Self { keep_recent }
    }
}

/// Find the byte range of the first `<tag>...</tag>` block in `text`,
/// tags included. Returns `None` if either tag is missing.
fn find_tagged_block(text: &str, tag: &str) -> Option<std::ops::Range<usize>> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)?;
    let after_open = &text[start + open.len()..];
    let close_offset = after_open.find(&close)?;
    let end = start + open.len() + close_offset + close.len();
    Some(start..end)
}

/// Strip the first `<analysis>...</analysis>` block from `text`. If
/// no analysis block exists the original text is returned unchanged.
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

/// Parse the summarizer response per the new contract:
/// 1. Strip the `<analysis>...</analysis>` block.
/// 2. If the remainder contains a `<summary>...</summary>` block,
///    return it verbatim (tags included).
/// 3. Otherwise, return whatever non-empty leftover is left after
///    removing the analysis block.
///
/// Returns `None` only when the leftover is empty / whitespace, in
/// which case the caller should fall back to truncation.
fn parse_summary_response(text: &str) -> Option<String> {
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

#[async_trait]
impl CompressionStrategy for Summarize {
    async fn compress(
        &self,
        messages: &[ChatMessage],
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

        // Gating lives in `ContextManager`; we always run.

        // Send the full conversation as-is, then append the
        // summarisation instruction as a trailing user turn.
        let mut request_messages: Vec<ChatMessage> = messages.to_vec();
        request_messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(SUMMARIZE_INSTRUCTION.to_string())],
        });
        let request = ChatRequest {
            messages: request_messages,
            temperature: None,
            tools: Vec::new(),
        };

        // Deterministic fallback used on transport failure or when
        // the response is empty after stripping the analysis block:
        // keep the system messages plus the last `keep_recent`
        // non-system messages, pair-preserved so we don't sever a
        // tool_use / tool_result pair. Even though we kept some
        // history here, we still flag `replaced_full_history: true`
        // because the strategy *attempted* a summary-style replacement
        // and the manager should re-broadcast the authoritative skill
        // reminder regardless of how the truncation tail looks.
        let fallback = || {
            let mut out = system_msgs.clone();
            let initial_split = non_system.len().saturating_sub(self.keep_recent);
            let split = pair_preserving_cut(&non_system, initial_split);
            out.extend_from_slice(&non_system[split..]);
            out
        };

        match chat(request).await {
            Ok(response) => match parse_summary_response(&response.content) {
                Some(content) => {
                    let mut new_messages = system_msgs;
                    new_messages.push(ChatMessage {
                        role: Role::User,
                        content: vec![ContentBlock::Text(content)],
                    });
                    Ok(CompressOutput::Replaced {
                        messages: new_messages,
                        replaced_full_history: true,
                    })
                }
                None => {
                    warn!(
                        "summarizer response empty after stripping analysis; falling back to truncation"
                    );
                    Ok(CompressOutput::Replaced {
                        messages: fallback(),
                        replaced_full_history: true,
                    })
                }
            },
            Err(e) => {
                warn!(error = %e, "summarization failed; falling back to truncation");
                Ok(CompressOutput::Replaced {
                    messages: fallback(),
                    replaced_full_history: true,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_llm::{LlmResponse, TokenUsage};

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

    /// Replaced shape: `[system, user(<summary>...</summary>)]` with
    /// `replaced_full_history: true`. The summary block must be
    /// carried over verbatim with its tags. Skill trailer attachment
    /// is `ContextManager`'s responsibility, not the strategy's.
    #[tokio::test]
    async fn produces_replaced_with_summary_block() {
        let strategy = Summarize::new(2);

        let messages = vec![
            make_msg(Role::System, "system prompt"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        let response = "<analysis>thinking</analysis>\n<summary>SUMMARY CONTENT</summary>";
        match strategy
            .compress(&messages, ok_chat(response))
            .await
            .unwrap()
        {
            CompressOutput::Replaced {
                messages: new_messages,
                replaced_full_history,
            } => {
                assert!(replaced_full_history);
                assert_eq!(new_messages.len(), 2);
                assert_eq!(new_messages[0].role, Role::System);
                assert_eq!(new_messages[1].role, Role::User);
                if let ContentBlock::Text(t) = &new_messages[1].content[0] {
                    assert_eq!(t, "<summary>SUMMARY CONTENT</summary>");
                } else {
                    panic!("expected text content");
                }
            }
            _ => panic!("expected Replaced"),
        }
    }

    /// Multiple system messages must all survive on the front, with
    /// a single user(summary) message appended.
    #[tokio::test]
    async fn preserves_all_system_messages() {
        let strategy = Summarize::new(1);

        let messages = vec![
            make_msg(Role::System, "sys 1"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::System, "sys 2"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
        ];

        let response = "<analysis>x</analysis><summary>S</summary>";
        match strategy
            .compress(&messages, ok_chat(response))
            .await
            .unwrap()
        {
            CompressOutput::Replaced {
                messages: new_messages,
                ..
            } => {
                assert_eq!(new_messages.len(), 3);
                assert_eq!(new_messages[0].role, Role::System);
                assert_eq!(new_messages[1].role, Role::System);
                assert_eq!(new_messages[2].role, Role::User);
                if let ContentBlock::Text(t) = &new_messages[2].content[0] {
                    assert_eq!(t, "<summary>S</summary>");
                } else {
                    panic!("expected text content");
                }
            }
            _ => panic!("expected Replaced"),
        }
    }

    /// On chat error the strategy falls back to a Truncate-equivalent
    /// slice. The flag stays `true` so the manager still re-broadcasts
    /// the skill reminder — the user-visible failure mode is "summary
    /// didn't land", not "skill list went stale".
    #[tokio::test]
    async fn chat_error_falls_back_to_truncation() {
        let strategy = Summarize::new(2);

        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        match strategy.compress(&messages, err_chat()).await.unwrap() {
            CompressOutput::Replaced {
                messages: new_messages,
                replaced_full_history,
            } => {
                assert!(replaced_full_history);
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

    /// Response that is *only* an analysis block (no leftover
    /// content) falls back to truncation — there's nothing usable
    /// after stripping `<analysis>`.
    #[tokio::test]
    async fn analysis_only_response_falls_back_to_truncation() {
        let strategy = Summarize::new(2);

        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        match strategy
            .compress(
                &messages,
                ok_chat("<analysis>only analysis, no summary</analysis>"),
            )
            .await
            .unwrap()
        {
            CompressOutput::Replaced {
                messages: new_messages,
                ..
            } => {
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

    /// Response with an analysis block followed by raw text (no
    /// `<summary>` tags): the analysis is stripped and the leftover
    /// content is used verbatim as the summary message body.
    #[tokio::test]
    async fn missing_summary_uses_leftover_after_analysis() {
        let strategy = Summarize::new(2);

        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        let response = "<analysis>thinking out loud</analysis>\nraw summary text without tags";
        match strategy
            .compress(&messages, ok_chat(response))
            .await
            .unwrap()
        {
            CompressOutput::Replaced {
                messages: new_messages,
                ..
            } => {
                assert_eq!(new_messages.len(), 2);
                assert_eq!(new_messages[0].role, Role::System);
                assert_eq!(new_messages[1].role, Role::User);
                if let ContentBlock::Text(t) = &new_messages[1].content[0] {
                    assert_eq!(t, "raw summary text without tags");
                } else {
                    panic!("expected text content");
                }
            }
            _ => panic!("expected Replaced"),
        }
    }

    /// Response with no analysis and no summary tags: the entire
    /// trimmed content becomes the summary body.
    #[tokio::test]
    async fn no_tags_at_all_uses_whole_response() {
        let strategy = Summarize::new(2);

        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        match strategy
            .compress(&messages, ok_chat("just plain summary content"))
            .await
            .unwrap()
        {
            CompressOutput::Replaced {
                messages: new_messages,
                ..
            } => {
                assert_eq!(new_messages.len(), 2);
                if let ContentBlock::Text(t) = &new_messages[1].content[0] {
                    assert_eq!(t, "just plain summary content");
                } else {
                    panic!("expected text content");
                }
            }
            _ => panic!("expected Replaced"),
        }
    }

    /// A wholly-empty payload falls back to the truncate-equivalent
    /// slice.
    #[tokio::test]
    async fn empty_summary_block_falls_back_to_truncation() {
        let strategy = Summarize::new(2);

        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "msg 1"),
            make_msg(Role::Assistant, "reply 1"),
            make_msg(Role::User, "msg 2"),
            make_msg(Role::Assistant, "reply 2"),
            make_msg(Role::User, "msg 3"),
        ];

        match strategy
            .compress(&messages, ok_chat("   \n  "))
            .await
            .unwrap()
        {
            CompressOutput::Replaced {
                messages: new_messages,
                ..
            } => {
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

    /// `keep_recent` no longer gates the strategy; a `/compact` on a
    /// short conversation must still reach the LLM.
    #[tokio::test]
    async fn runs_even_when_under_keep_recent() {
        let strategy = Summarize::new(10);

        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "hello"),
            make_msg(Role::Assistant, "hi"),
        ];

        match strategy
            .compress(
                &messages,
                ok_chat("<analysis>a</analysis><summary>S</summary>"),
            )
            .await
            .unwrap()
        {
            CompressOutput::Replaced {
                messages: new_messages,
                replaced_full_history,
            } => {
                assert!(replaced_full_history);
                assert_eq!(new_messages.len(), 2);
                assert_eq!(new_messages[0].role, Role::System);
                if let ContentBlock::Text(t) = &new_messages[1].content[0] {
                    assert_eq!(t, "<summary>S</summary>");
                } else {
                    panic!("expected text content");
                }
            }
            _ => panic!("expected Replaced — manager owns the budget gate now"),
        }
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
        // Unterminated open tag is left alone — we only strip a
        // properly closed block.
        assert_eq!(
            strip_analysis_block("<analysis>no close"),
            "<analysis>no close"
        );
    }
}
