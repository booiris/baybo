//! Framing for a board issue's brief.

/// Opening tag of the framed brief. The issue number rides in it, so a
/// transcript read on its own still says which card the work belongs to.
pub const ISSUE_TAG_PREFIX: &str = "[issue #";

const FRAMING_BODY: &str = r#"You are working on this issue as its assignee. This is a task from a
project board, not a message from a person — nobody is waiting at a
keyboard for a reply. Do the work, and let what you write be the record
of it: what you changed, what you found, and anything the operator has to
decide."#;

const INSTRUCTION_LABEL: &str = "The issue:";

const CHECKOUT_BODY: &str = r#"Your checkout for this issue is:

  {{checkout}}

Commands run there by default and your branch is already checked out, so
work in place: no `cd`, and no clone. Other issues have their own
checkouts of the same repository — treat anything outside this one as
somebody else's."#;

/// Frame an issue's brief for the run that will execute it.
pub fn frame_issue_brief(number: i64, checkout: &str, brief: &str) -> String {
    let where_it_works = CHECKOUT_BODY.replace("{{checkout}}", checkout);
    format!(
        "{ISSUE_TAG_PREFIX}{number}] {FRAMING_BODY}\n\n{where_it_works}\n\n{INSTRUCTION_LABEL}\n{brief}"
    )
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
        // The run is told where it is, because nothing else tells it: the
        // Bash description is rendered per process and names the workspace
        // work dir for every session.
        assert!(
            framed.contains("/ws/work/projects/p/7"),
            "the framing must name the checkout: {framed}"
        );
    }
}
