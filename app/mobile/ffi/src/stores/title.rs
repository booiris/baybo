//! The conversation-rename rules, shared by both shells and by the gateway.
//!
//! Four homes existed before this: `baybo_session::validate_session_title` (the
//! authority), `app/ios`'s `RenameTitle.swift`, `app/web`'s `renameTitle.ts`,
//! and — once Android landed — a fourth. The whitespace collapse and the cap
//! now live once in `baybo_model`; this exposes the CLIENT policy over them.
//!
//! Client and server differ deliberately. The server REJECTS a title that is
//! empty or over-long; a client TRUNCATES, because it is keeping the user from
//! typing into a rejection, and because the value it sends is also the value it
//! renders optimistically — the endpoint stores the normalized form and
//! broadcasts that back, so a row showing the raw draft would visibly rewrite
//! itself moments later.
//!
//! `app/web`'s TypeScript copy stays a mirror; nothing can be shared across
//! that boundary, which is why its port-fidelity test exists.

use baybo_model::{cap_session_title, collapse_session_title};

/// Clip to the cap, counting the way the server counts.
#[uniffi::export]
pub fn session_title_cap(text: String) -> String {
    cap_session_title(&text)
}

/// What the gateway will store for `text`: whitespace collapsed, then capped.
#[uniffi::export]
pub fn session_title_normalized(text: String) -> String {
    cap_session_title(&collapse_session_title(&text))
}

/// The draft the editor opens with: whatever the row currently shows.
///
/// Capped, because a title minted server-side (a cron fire's) predates no such
/// bound — seeding one whole would produce a draft the server refuses even if
/// the user changes nothing.
#[uniffi::export]
pub fn session_title_seed(title: Option<String>, user_text: Option<String>) -> String {
    cap_session_title(&title.or(user_text).unwrap_or_default())
}

/// The title to send, or `None` to send nothing.
///
/// Compared against the SEED rather than the row's stored title: an untouched
/// editor must commit nothing, and for an untitled row the seed is the user's
/// last message — committing that would rename the conversation to its own
/// preview *and* settle it against the auto-titler, which only ever writes into
/// a conversation that still has no title.
#[uniffi::export]
pub fn session_title_to_commit(draft: String, seed: String) -> Option<String> {
    let title = session_title_normalized(draft);
    if title.is_empty() || title == session_title_normalized(seed) {
        return None;
    }
    Some(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from `app/ios/Tests/RenameTitleTests.swift`, which keeps running
    /// against the Swift forwarder. Both suites assert the same vectors, so a
    /// drift between the core and the shell is a failure on one side or the
    /// other rather than a silent divergence.
    #[test]
    fn caps_at_the_servers_length_counted_in_scalars() {
        assert_eq!(session_title_cap("a".repeat(90)).chars().count(), 80);
        assert_eq!(session_title_cap("短标题".into()), "短标题");
        // A flag emoji is ONE grapheme cluster and TWO scalars. Counting
        // clusters here would let 80 flags (160 scalars) past a client that
        // the server then rejects.
        let flags = "🇯🇵".repeat(50);
        assert_eq!(session_title_cap(flags).chars().count(), 80);
    }

    #[test]
    fn normalized_collapses_interior_whitespace_and_trims() {
        assert_eq!(
            session_title_normalized("  Fix   the\nlogin\tredirect  ".into()),
            "Fix the login redirect"
        );
        assert_eq!(session_title_normalized("   ".into()), "");
        assert_eq!(session_title_normalized("\n\t".into()), "");
    }

    #[test]
    fn a_blank_draft_commits_nothing() {
        assert_eq!(session_title_to_commit("".into(), "Old".into()), None);
        assert_eq!(session_title_to_commit("   \n ".into(), "Old".into()), None);
    }

    #[test]
    fn an_untouched_draft_commits_nothing() {
        assert_eq!(
            session_title_to_commit("Old name".into(), "Old name".into()),
            None
        );
        // Whitespace-only edits are not edits: both sides normalize first.
        assert_eq!(
            session_title_to_commit("  Old   name ".into(), "Old name".into()),
            None
        );
    }

    #[test]
    fn a_changed_draft_commits_its_normalized_form() {
        assert_eq!(
            session_title_to_commit("  New   name ".into(), "Old".into()),
            Some("New name".to_string())
        );
    }

    /// The untitled row's seed is the user's own last message, so opening the
    /// dialog and closing it must not rename the conversation to its preview.
    #[test]
    fn an_untitled_row_seeds_from_its_last_message_and_commits_nothing_untouched() {
        let seed = session_title_seed(None, Some("what is the answer".into()));
        assert_eq!(seed, "what is the answer");
        assert_eq!(session_title_to_commit(seed.clone(), seed), None);
    }

    #[test]
    fn a_row_with_nothing_to_show_seeds_empty() {
        assert_eq!(session_title_seed(None, None), "");
    }

    #[test]
    fn a_server_minted_title_is_capped_before_it_seeds_an_editor() {
        let long = "x".repeat(120);
        assert_eq!(
            session_title_seed(Some(long), None).chars().count(),
            80,
            "an over-long stored title must not seed a draft the server refuses"
        );
    }
}
