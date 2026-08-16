//! Marker appended to a cancelled turn's salvaged partial assistant content.
//!
//! When a `/stop` aborts an in-flight LLM call, the agent loop persists the
//! partial reply / reasoning produced so far so the cancelled turn survives a
//! page reload. [`SUFFIX`] is folded into that salvaged text so the *next*
//! request reads the truncated turn back as incomplete rather than as a
//! deliberate stopping point. It is model-facing framing, so display surfaces
//! strip it via [`strip_marker`] before showing the partial reply — the same
//! "frame for the model, strip for the user" contract [`super::cron`] uses.

/// Folded into a cancelled turn's salvaged trailing text. The leading blank
/// line separates it from the partial reply in the model's view. When there
/// is no trailing text block to fold it into (a thinking-only salvage), it is
/// emitted as its own block via [`marker_block_text`].
pub const SUFFIX: &str = "\n\n[This turn was cancelled before it finished — the reply and any reasoning above are incomplete.]";

/// Synthetic result for a cancelled in-flight call whose side effects are unknown.
pub const TOOL_RESULT_BODY: &str = "[the turn was cancelled while this tool call was still running; it may or may not have completed its side effects]";

/// The marker as a standalone block body — [`SUFFIX`] without its leading
/// blank line, for the thinking-only salvage that has no text block to append
/// to.
pub fn marker_block_text() -> String {
    SUFFIX.trim_start().to_string()
}

/// Strip the cancelled-turn marker for display so a reconstructed reply bubble
/// shows only the agent's partial output, not the model-facing note. Handles
/// both the folded-into-text form ([`SUFFIX`]) and the standalone-block form
/// ([`marker_block_text`]); returns the input unchanged when no marker is
/// present.
pub fn strip_marker(text: &str) -> &str {
    if let Some(stripped) = text.strip_suffix(SUFFIX) {
        return stripped.trim_end();
    }
    if let Some(stripped) = text.strip_suffix(SUFFIX.trim_start()) {
        return stripped.trim_end();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_folded_suffix() {
        let displayed = format!("a b-tree is{SUFFIX}");
        assert_eq!(strip_marker(&displayed), "a b-tree is");
    }

    #[test]
    fn strips_standalone_marker_block() {
        assert_eq!(strip_marker(&marker_block_text()), "");
    }

    #[test]
    fn leaves_unmarked_text_untouched() {
        assert_eq!(strip_marker("a finished answer"), "a finished answer");
    }

    #[test]
    fn the_cancel_fill_does_not_claim_a_crash() {
        assert!(!TOOL_RESULT_BODY.is_empty());
        assert!(!TOOL_RESULT_BODY.contains("crash"));
        assert!(!TOOL_RESULT_BODY.contains("restart"));
        assert!(TOOL_RESULT_BODY.contains("cancelled"));
    }
}
