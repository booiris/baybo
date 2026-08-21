//! Framing for a board issue's brief.

/// Opening tag of the framed brief. The issue number rides in it, so a
/// transcript read on its own still says which card the work belongs to.
pub const ISSUE_TAG_PREFIX: &str = "[issue #";

const FRAMING_BODY: &str = r#"You are working on this issue as its assignee. This is a task from a
project board, not a message from a person — nobody is waiting at a
keyboard for a reply. Do the work, and let what you write be the record
of it: what you changed, what you found, and anything the operator has to
decide. Read the brief below before fetching this card again."#;

const INSTRUCTION_LABEL: &str = "The issue:";

const CHECKOUT_BODY: &str = r#"Your checkout for this issue is:

  {{checkout}}

Its branch is already checked out; do not clone the repository again.
Other issues have their own checkouts of the same repository — treat
anything outside this one as somebody else's."#;

/// Frame an issue's brief for the run that will execute it.
pub fn frame_issue_brief(number: i64, checkout: &str, brief: &str) -> String {
    let where_it_works = CHECKOUT_BODY.replace("{{checkout}}", checkout);
    format!(
        "{ISSUE_TAG_PREFIX}{number}] {FRAMING_BODY}\n\n{where_it_works}\n\n{INSTRUCTION_LABEL}\n{brief}"
    )
}

/// The card's own ask, with the framing this module wrapped it in taken back
/// off — for a surface that shows a run's brief to a **person**.
///
/// The framing is written for the model: who it is, that nobody is waiting at
/// a keyboard, where its checkout is. An operator reading a run already knows
/// all of that, and rendering it as the opening bubble buries the one line
/// they came for under six hundred characters of machinery.
///
/// Split on [`INSTRUCTION_LABEL`], which the framer deliberately puts last so
/// that the instruction is the tail. A row that carries no label is handed
/// back whole: a brief framed by some older shape is still the ask, and
/// guessing where it ends would be worse than showing it.
pub fn unframe_issue_brief(framed: &str) -> &str {
    match framed.split_once(&format!("\n{INSTRUCTION_LABEL}\n")) {
        Some((_framing, ask)) => ask,
        None => framed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_names_the_card_and_keeps_the_brief_last() {
        let framed = frame_issue_brief(7, "/ws/work/projects/p/7", "Fix the reconnect storm");
        assert!(framed.starts_with("[issue #7]"), "{framed}");
        assert!(
            framed.ends_with("The issue:\nFix the reconnect storm"),
            "the instruction is the tail, so a reader knows where framing ends: {framed}"
        );
        assert!(
            framed.contains("/ws/work/projects/p/7"),
            "the framing must name the checkout: {framed}"
        );
    }

    #[test]
    fn a_reader_is_given_the_ask_and_none_of_the_framing() {
        let framed = frame_issue_brief(7, "/ws/work/projects/p/7", "Fix the reconnect storm");
        assert_eq!(unframe_issue_brief(&framed), "Fix the reconnect storm");

        // A brief holding the label itself keeps everything past the FIRST
        // one, which is the one the framer wrote.
        let quoting = frame_issue_brief(7, "/ws", "See below.\nThe issue:\nis subtle");
        assert_eq!(
            unframe_issue_brief(&quoting),
            "See below.\nThe issue:\nis subtle"
        );

        // Anything this module did not frame is the ask already.
        assert_eq!(unframe_issue_brief("just do it"), "just do it");
    }
}
