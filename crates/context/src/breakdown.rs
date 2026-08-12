//! Where an LLM call's input tokens went.
//!
//! The provider reports one number — the whole prompt. That is enough to bill
//! a call and useless for the question a reader of the trace actually has:
//! *what is eating my context?* This splits the recorded input into the parts
//! it was assembled from, counting each with the same [`TiktokenTokenizer`]
//! the live [`ContextManager`](crate::ContextManager) budgets against.
//!
//! **The split is an estimate, the total is not.** tiktoken is not Anthropic's
//! tokenizer (see [`TiktokenTokenizer`]'s note, and `TokenCalibration`, which
//! exists precisely to correct that drift at runtime), so a consumer should
//! present the span's recorded `input_tokens` as the total and treat these
//! numbers as proportions of it.

use baybo_model::{ChatMessage, ContentBlock, MessageSource, Role};
use baybo_trace::LlmToolDefinition;
use serde::{Deserialize, Serialize};

use crate::tokenizer::{TiktokenTokenizer, Tokenizer};

/// Which part of the assembled context a segment belongs to.
///
/// Closed enum, derived from the `(role, source)` pair a message already
/// carries — no re-classification by sniffing content. The variants are the
/// distinctions a reader can act on: a bloated system prompt, a tool set that
/// outgrew its usefulness, one enormous tool result, memories that keep
/// getting recalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPart {
    /// The leading `Role::System` row, plus the update notices that amend it.
    SystemPrompt,
    /// The tool definitions the call offered.
    Tools,
    /// The standing skill listing and its update notices.
    Skills,
    /// Memories recalled from long-term storage for this turn.
    Memory,
    /// A genuine human message (including a mid-turn interjection).
    User,
    /// A cron fire's framed prompt, or a fire result delivered back.
    Cron,
    /// Model output carried forward as history.
    Assistant,
    /// Tool results in the transcript.
    ToolResult,
    /// Everything else the agent injects: task prompts, reminders, framing.
    Agent,
    /// Images, audio, and files — priced by the provider's media rules rather
    /// than by a tokenizer, so they are kept apart from the text estimate.
    Media,
}

impl ContextPart {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContextPart::SystemPrompt => "system_prompt",
            ContextPart::Tools => "tools",
            ContextPart::Skills => "skills",
            ContextPart::Memory => "memory",
            ContextPart::User => "user",
            ContextPart::Cron => "cron",
            ContextPart::Assistant => "assistant",
            ContextPart::ToolResult => "tool_result",
            ContextPart::Agent => "agent",
            ContextPart::Media => "media",
        }
    }
}

/// One contiguous piece of the context: a message, or the tool set.
///
/// Emitted in the order the model saw them, so a consumer can lay them out as
/// a timeline without re-sorting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSegment {
    pub part: ContextPart,
    /// Short human label — a tool name, a role, the system prompt.
    pub label: String,
    /// Estimated tokens. Text only; a message's media price rides in its own
    /// [`ContextPart::Media`] segment.
    pub tokens: usize,
    /// Position in the input, 0-based. Two segments can share one index when a
    /// message contributes both text and media.
    pub index: usize,
}

/// The full split of one call's input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextBreakdown {
    pub segments: Vec<ContextSegment>,
    /// Sum of every segment. The consumer's denominator when scaling the split
    /// onto the provider's reported total.
    pub estimated_total_tokens: usize,
}

/// How a message's `(role, source)` maps onto a context part.
///
/// Source wins over role wherever the two disagree, because the sources that
/// matter here (`recalled_memory`, `skill_listing`, `system_prompt_update`)
/// all ride as framed `Role::User` rows deliberately — a system row would
/// re-assert itself on every later turn — so classifying by role alone would
/// file all of them under "user".
fn classify(message: &ChatMessage) -> ContextPart {
    match message.source() {
        MessageSource::RecalledMemory => ContextPart::Memory,
        MessageSource::SkillListing | MessageSource::SkillsUpdate => ContextPart::Skills,
        MessageSource::SystemPromptUpdate => ContextPart::SystemPrompt,
        MessageSource::Cron | MessageSource::CronNotification => ContextPart::Cron,
        MessageSource::User | MessageSource::UserInterjection => ContextPart::User,
        // A board card handed to a run: framed and assembled by the board, not
        // typed by anyone, so not `User`. [`ContextPart::Agent`]'s own
        // definition — "task prompts, reminders, framing" — is what this is.
        //
        // Worth revisiting rather than settling: by the `Cron` precedent above,
        // a framed prompt that drives an autonomous turn earned its own bucket,
        // and an issue brief now carries the card's files as well as its prose.
        // Splitting it out is a public enum change that reaches the analytics
        // UI, so it is not something to slip into a merge.
        MessageSource::IssueBrief => ContextPart::Agent,
        MessageSource::Agent => match message.role {
            Role::System => ContextPart::SystemPrompt,
            Role::Assistant => ContextPart::Assistant,
            Role::Tool => ContextPart::ToolResult,
            // A tool result can ride on a `Role::User` row (providers that
            // have no tool role take it that way), so the block shape is the
            // tiebreaker — otherwise a transcript full of tool output would
            // be filed as agent framing.
            Role::User if carries_tool_result(message) => ContextPart::ToolResult,
            Role::User => ContextPart::Agent,
        },
    }
}

fn carries_tool_result(message: &ChatMessage) -> bool {
    message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

/// A label short enough for a legend row. Tool results get theirs from
/// [`named_tool_result`], which can name the tool that produced them.
fn label_for(part: ContextPart) -> String {
    match part {
        ContextPart::SystemPrompt => "System prompt".to_string(),
        ContextPart::Skills => "Skills".to_string(),
        ContextPart::Memory => "Recalled memory".to_string(),
        ContextPart::User => "User message".to_string(),
        ContextPart::Cron => "Cron".to_string(),
        ContextPart::Assistant => "Assistant".to_string(),
        ContextPart::ToolResult => "Tool result".to_string(),
        ContextPart::Agent => "Agent-injected".to_string(),
        ContextPart::Tools | ContextPart::Media => part.as_str().to_string(),
    }
}

/// Split `messages` (the exact slice the model saw) and `tools` (the set it
/// was offered) into per-part token estimates.
///
/// `model_id` only picks the BPE encoding; it is not a calibration key.
pub fn context_breakdown(
    model_id: &str,
    messages: &[ChatMessage],
    tools: &[LlmToolDefinition],
) -> ContextBreakdown {
    let tokenizer = TiktokenTokenizer::for_model(model_id);
    let mut segments = Vec::with_capacity(messages.len() + 1);

    if !tools.is_empty() {
        // Counted as the serialized definition, which is what goes on the
        // wire — a name-plus-description estimate would miss the schema, and
        // the schema is most of the weight.
        let tokens: usize = tools
            .iter()
            .map(|t| {
                let schema = serde_json::to_string(&t.parameters_schema).unwrap_or_default();
                tokenizer.count_text(&t.name)
                    + tokenizer.count_text(&t.description)
                    + tokenizer.count_text(&schema)
            })
            .sum();
        segments.push(ContextSegment {
            part: ContextPart::Tools,
            label: format!("{} tool definitions", tools.len()),
            tokens,
            index: 0,
        });
    }

    // Tool results name only a `tool_use_id`; the name lives on the `ToolUse`
    // block that produced it, earlier in the same transcript. One pass builds
    // the lookup so a result can be labelled with the tool a reader recognises.
    let mut tool_names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                tool_names.insert(id.as_str(), name.as_str());
            }
        }
    }

    for (index, message) in messages.iter().enumerate() {
        let part = classify(message);
        let media = tokenizer.count_message_media(message);
        let text = tokenizer.count_message(message).saturating_sub(media);
        if text > 0 {
            let label = match part {
                ContextPart::ToolResult => named_tool_result(message, &tool_names),
                _ => label_for(part),
            };
            segments.push(ContextSegment {
                part,
                label,
                tokens: text,
                index,
            });
        }
        if media > 0 {
            segments.push(ContextSegment {
                part: ContextPart::Media,
                label: media_label(message),
                tokens: media,
                index,
            });
        }
    }

    let estimated_total_tokens = segments.iter().map(|s| s.tokens).sum();
    ContextBreakdown {
        segments,
        estimated_total_tokens,
    }
}

fn named_tool_result(
    message: &ChatMessage,
    tool_names: &std::collections::HashMap<&str, &str>,
) -> String {
    for block in &message.content {
        if let ContentBlock::ToolResult { tool_use_id, .. } = block
            && let Some(name) = tool_names.get(tool_use_id.as_str())
        {
            return format!("{name} result");
        }
    }
    "Tool result".to_string()
}

fn media_label(message: &ChatMessage) -> String {
    let mut images = 0;
    let mut files = 0;
    let mut audio = 0;
    for block in &message.content {
        match block {
            ContentBlock::Image { .. } => images += 1,
            ContentBlock::File { .. } => files += 1,
            ContentBlock::Audio { .. } => audio += 1,
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if images > 0 {
        parts.push(format!("{images} image(s)"));
    }
    if files > 0 {
        parts.push(format!("{files} file(s)"));
    }
    if audio > 0 {
        parts.push(format!("{audio} audio"));
    }
    if parts.is_empty() {
        "Attachment".to_string()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{BlobRef, ThinkingContent};

    const MODEL: &str = "claude-sonnet-4-6";

    fn text(t: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(t.into())]
    }

    fn tool(name: &str) -> LlmToolDefinition {
        LlmToolDefinition {
            name: name.into(),
            description: "does a thing".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
            }),
        }
    }

    #[test]
    fn the_tool_set_is_one_segment_counted_from_its_schemas() {
        // The schema is most of a definition's weight; counting name +
        // description alone would report a tool set as near-free and send a
        // reader hunting through their transcript instead.
        let with_schema = context_breakdown(MODEL, &[], &[tool("bash")]);
        let bare = context_breakdown(
            MODEL,
            &[],
            &[LlmToolDefinition {
                name: "bash".into(),
                description: "does a thing".into(),
                parameters_schema: serde_json::Value::Null,
            }],
        );
        assert_eq!(with_schema.segments.len(), 1);
        assert_eq!(with_schema.segments[0].part, ContextPart::Tools);
        assert!(with_schema.segments[0].tokens > bare.segments[0].tokens);
    }

    #[test]
    fn no_tools_means_no_tools_segment() {
        let out = context_breakdown(MODEL, &[ChatMessage::user(text("hi"))], &[]);
        assert!(out.segments.iter().all(|s| s.part != ContextPart::Tools));
    }

    #[test]
    fn source_beats_role_for_the_framed_user_rows() {
        // Recalled memory, the skill listing, and system-prompt updates all
        // ride as `Role::User` deliberately — a system row would re-assert
        // itself every turn. Classifying by role would file all three as
        // "user" and hide the three things most likely to be bloating a
        // context.
        let messages = [
            ChatMessage::recalled_memory(text("remembered")),
            ChatMessage::skill_listing(text("skills")),
            ChatMessage::system_prompt_update(text("moved")),
            ChatMessage::user(text("the actual question")),
        ];
        for m in &messages {
            assert_eq!(m.role, Role::User, "the premise: all four are user rows");
        }
        let parts: Vec<ContextPart> = context_breakdown(MODEL, &messages, &[])
            .segments
            .iter()
            .map(|s| s.part)
            .collect();
        assert_eq!(
            parts,
            vec![
                ContextPart::Memory,
                ContextPart::Skills,
                ContextPart::SystemPrompt,
                ContextPart::User,
            ]
        );
    }

    #[test]
    fn a_tool_result_on_a_user_row_is_still_a_tool_result() {
        // Providers without a tool role take results on a user row. Reading
        // the role alone would file a transcript full of tool output as agent
        // framing — the single largest misattribution available here.
        let message = ChatMessage::agent_context(vec![ContentBlock::ToolResult {
            tool_use_id: "call-1".into(),
            content: "output".into(),
            meta: None,
        }]);
        assert_eq!(message.role, Role::User, "the premise: it is a user row");
        let out = context_breakdown(MODEL, &[message], &[]);
        assert_eq!(out.segments[0].part, ContextPart::ToolResult);
    }

    #[test]
    fn a_tool_result_is_labelled_with_the_tool_that_produced_it() {
        // `ToolResult` carries only an id, so the name has to come from the
        // `ToolUse` earlier in the transcript.
        let call = ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
            signature: None,
        }]);
        let result = ChatMessage::tool_result("call-1".into(), "output".into());
        let out = context_breakdown(MODEL, &[call, result], &[]);
        let labels: Vec<&str> = out.segments.iter().map(|s| s.label.as_str()).collect();
        assert!(
            labels.contains(&"bash result"),
            "expected the tool name in the labels, got {labels:?}"
        );
    }

    #[test]
    fn media_is_its_own_segment_not_folded_into_the_text_estimate() {
        // An image's price is the provider's tile arithmetic, not a tokenizer
        // count. Folding it into the text number would make the split look
        // like an estimate that is badly wrong rather than one that is
        // mostly right with a priced attachment beside it.
        let message = ChatMessage::user(vec![
            ContentBlock::Text("look at this".into()),
            ContentBlock::Image {
                blob: BlobRef {
                    blob_id: "b1".into(),
                },
                mime_type: "image/png".into(),
                filename: None,
                width: Some(1024),
                height: Some(768),
            },
        ]);
        let out = context_breakdown(MODEL, &[message], &[]);
        let parts: Vec<ContextPart> = out.segments.iter().map(|s| s.part).collect();
        assert_eq!(parts, vec![ContextPart::User, ContextPart::Media]);
        assert!(out.segments.iter().all(|s| s.tokens > 0));
        assert_eq!(
            out.estimated_total_tokens,
            out.segments.iter().map(|s| s.tokens).sum::<usize>()
        );
    }

    #[test]
    fn thinking_rides_with_the_assistant_turn_that_produced_it() {
        let message = ChatMessage::assistant(vec![ContentBlock::Thinking {
            id: None,
            content: vec![ThinkingContent::Text {
                text: "a long deliberation".into(),
                signature: None,
            }],
        }]);
        let out = context_breakdown(MODEL, &[message], &[]);
        assert_eq!(out.segments[0].part, ContextPart::Assistant);
    }

    #[test]
    fn segments_keep_the_order_the_model_saw() {
        let messages = [
            ChatMessage::system(text("you are a bot")),
            ChatMessage::user(text("hi")),
            ChatMessage::assistant(text("hello")),
        ];
        let out = context_breakdown(MODEL, &messages, &[tool("bash")]);
        let parts: Vec<ContextPart> = out.segments.iter().map(|s| s.part).collect();
        // Tools first: they precede the transcript on the wire.
        assert_eq!(
            parts,
            vec![
                ContextPart::Tools,
                ContextPart::SystemPrompt,
                ContextPart::User,
                ContextPart::Assistant,
            ]
        );
        assert_eq!(
            out.segments.iter().map(|s| s.index).collect::<Vec<_>>(),
            vec![0, 0, 1, 2]
        );
    }

    #[test]
    fn an_empty_input_breaks_down_to_nothing() {
        let out = context_breakdown(MODEL, &[], &[]);
        assert!(out.segments.is_empty());
        assert_eq!(out.estimated_total_tokens, 0);
    }
}
