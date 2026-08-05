//! Framing for a board issue's brief.
//!
//! The brief is what the assignee is asked to do, framed the way a cron
//! fire is: a tagged envelope so the model can tell an instruction it was
//! handed from something a person typed at it, and so an operator surface
//! can identify a run's input by provenance rather than by sniffing text.

/// Opening tag of the framed brief. The issue number rides in it, so a
/// transcript read on its own still says which card the work belongs to.
pub const ISSUE_TAG_PREFIX: &str = "[issue #";

const FRAMING_BODY: &str = r#"You are working on this issue as its assignee. This is a task from a
project board, not a message from a person — nobody is waiting at a
keyboard for a reply. Do the work, and let what you write be the record
of it: what you changed, what you found, and anything the operator has to
decide."#;

const INSTRUCTION_LABEL: &str = "The issue:";

/// Frame an issue's brief for the run that will execute it.
pub fn frame_issue_brief(number: i64, brief: &str) -> String {
    format!("{ISSUE_TAG_PREFIX}{number}] {FRAMING_BODY}\n\n{INSTRUCTION_LABEL}\n{brief}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_names_the_card_and_keeps_the_brief_last() {
        let framed = frame_issue_brief(7, "Fix the reconnect storm");
        assert!(framed.starts_with("[issue #7]"), "{framed}");
        assert!(
            framed.ends_with("The issue:\nFix the reconnect storm"),
            "the instruction is the tail, so a reader knows where framing ends: {framed}"
        );
    }
}
