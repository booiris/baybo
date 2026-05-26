//! Wire framing for a mid-turn user interjection ("steering").
//!
//! A message the user sends *while the agent loop is still working* is drained
//! at the next tool boundary and injected as a `Role::User` row stamped
//! [`aura_model::MessageSource::UserInterjection`]. The raw text is persisted
//! faithfully (a clean user bubble); this envelope is applied **wire-only** by
//! [`crate::ContextManager::messages_for_llm`] and re-derived from the source
//! flag on every call, so it survives compaction/rebuild and is never stored.
//!
//! The framing tells the model the message arrived mid-turn — steering, not a
//! reply to anything it asked — and how to weigh it against the in-progress
//! task. See `docs/mid-turn-user-interjection.md`.

const OPEN_TAG: &str = "<user_interjection>";
const CLOSE_TAG: &str = "</user_interjection>";

/// Framing preamble placed before the tagged block.
const FRAMING_BODY: &str = r#"The user sent the message(s) below while you were still working on their current request — they did not wait for you to finish, so treat this as a live interjection, not a reply to anything you asked.

- If it refines, corrects, or redirects what you are currently doing, fold it into your work now.
- If it is a new request unrelated to the current task, finish the current task first and then address it in this same turn — do not abandon the in-progress work.

Briefly acknowledge that you saw it in your reply."#;

/// Wrap one or more interjection message texts (in arrival order) in the
/// `<user_interjection>` steering envelope. Multiple messages are joined with a
/// blank line inside a single block.
///
/// Unlike [`crate::prompts::tool_output`], the body is **not** breakout-escaped
/// for a literal `</user_interjection>`: the envelope only frames the turn for
/// the model, and the content is the *user's own* message — the user is the
/// trusted principal of their own turn, not an untrusted external source, so
/// there is no boundary to forge against themselves. (Inbound content is still
/// leak-scanned at Router ingress before it ever reaches here.)
pub fn wrap_interjections(texts: &[String]) -> String {
    let joined = texts.join("\n\n");
    format!("{FRAMING_BODY}\n\n{OPEN_TAG}\n{joined}\n{CLOSE_TAG}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_single_message_with_framing_and_tags() {
        let out = wrap_interjections(&["actually, use TypeScript".to_string()]);
        assert!(out.contains("live interjection"));
        assert!(out.contains("finish the current task first"));
        assert!(
            out.contains("<user_interjection>\nactually, use TypeScript\n</user_interjection>")
        );
    }

    #[test]
    fn joins_multiple_messages_in_one_block() {
        let out = wrap_interjections(&["one".to_string(), "two".to_string()]);
        // One envelope, both messages inside, blank-line separated.
        assert_eq!(out.matches(OPEN_TAG).count(), 1);
        assert_eq!(out.matches(CLOSE_TAG).count(), 1);
        assert!(out.contains("one\n\ntwo"));
    }
}
