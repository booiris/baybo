//! Compression prompts: the summarize instruction handed to the model and
//! the continuation framing wrapped around the resulting summary.
//!
//! Relocated here so all model-facing prompt text lives under `prompts/`;
//! the compression *flow* stays in `compressor.rs` and imports these.

/// Intro paragraph framing the summary as continuation of an earlier
/// session. Mirrors Claude Code's compaction prompt.
pub(crate) const CONTINUATION_INTRO: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the\nearlier portion of the conversation.";

/// Closing paragraph instructing the model to resume work directly, without
/// acknowledging the summary or prefacing the reply.
///
/// Two variants because the claim about verbatim messages has to be true:
/// the compaction keeps a recent slice when it fits, and drops it when the
/// result would otherwise be no smaller than what it replaced.
pub(crate) const CONTINUATION_FOOTER_WITH_SLICE: &str =
    "The most recent messages are preserved verbatim below this summary.\n";
pub(crate) const CONTINUATION_FOOTER: &str = "Continue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening, do not preface with \"I'll continue\" or similar. Pick up the last task as if the break never happened.";

/// Trailing summary instruction. Tool invocation is disabled on the request;
/// the parser remains tolerant of older response shapes.
pub const SUMMARIZE_INSTRUCTION: &str = r#"Create a continuation summary of the conversation above. Respond with exactly one <summary> block and no text outside it.

Preserve what another model needs to continue the work without reopening settled questions:

1. Every user request, correction, constraint, and preference, noting what is complete, pending, superseded, or still ambiguous.
2. Work completed so far, key decisions and their reasons, and any failed approaches or errors that affect what should happen next.
3. Relevant files, symbols, commands, technical details, and current repository state. Quote exact code only when its exact text is necessary to continue.
4. The task in progress immediately before this summary, its remaining work, and the next action already implied by the user's request.

Recent messages may be preserved verbatim after the summary. Capture their unresolved state, but do not quote or reproduce them. Do not invent next steps or revive completed work.

<summary>
...
</summary>"#;
