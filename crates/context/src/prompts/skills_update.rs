//! Framing for the notice that reconciles a stale skill listing with the
//! registry it was rendered from.
//!
//! The listing is written once, at seed, as a `<system-reminder>` row — and
//! persisted. Nothing refreshes it mid-session: the post-compaction trailer
//! re-broadcasts a fresh one, and until then the conversation is reading a
//! snapshot. The registry, meanwhile, is mutated at runtime by
//! `SkillInstall`/`SkillUninstall` and by the operator dashboard's refresh, so
//! a long conversation ends up naming skills that no longer exist and blind to
//! ones that do. **The model itself opens that gap**: a conversation that
//! installs a skill cannot see it, and every other live conversation desyncs at
//! the same moment.
//!
//! Appended, never written over the listing row. That row sits immediately
//! after `messages[0]` and `merge_for_llm` folds it into the first user
//! message, so it is inside the prompt-cache prefix — rewriting it costs the
//! provider's cached copy of the whole transcript, exactly as rewriting
//! `messages[0]` would. The same reasoning as
//! [`crate::prompts::system_prompt_update`], and for the same reason each
//! update is a **complete delta against the listing row** rather than against
//! the update before it, with the framing telling the model the last one wins.
//!
//! **A separate envelope from the system-prompt notice, deliberately.** That
//! one states it does not escape its payload, because every byte comes from the
//! workspace's own identity files — a hostile `SOUL.md` has already won by
//! being the system prompt. A skill is not that: it is third-party content an
//! operator or the model installed, so its `description` is untrusted text and
//! `baybo_skills::render::render_skill_listing` neutralises the envelopes it
//! rides in. Folding the two would have quietly extended the trusted-payload
//! claim to bytes that do not deserve it.

use crate::prompts::line_diff::{DIFF_ATTR, unified_diff};

const OPEN_TAG: &str = "<skills_update>";
const CLOSE_TAG: &str = "</skills_update>";

/// Tag for the listing block. No `path` — the listing is derived from a
/// registry spanning several directories — and no `SectionTag`, because it does
/// not live in `messages[0]`.
const SKILLS_TAG: &str = "skills";

/// The whole framing, in one paragraph. Three claims, each load-bearing.
///
/// **Where the copy above is.** For the system-prompt notice the baseline is
/// the leading row and needs no locating; here it is a `<system-reminder>` row
/// further down, and nothing else in the request says so.
///
/// **What a `-` line means.** A removed skill is *uninstalled*, and calling it
/// fails. This is the skills analogue of the dead-path warning, and it is why a
/// removal is never communicated by absence: a model that reads an omission as
/// an abbreviation keeps planning around a skill that is gone.
///
/// **Latest wins.** Updates accumulate until a compaction retires them, and
/// each restates the whole delta against the listing row, so without the rule a
/// sequence of them reads as contradictory.
const FRAMING_BODY: &str = r#"The skills listed in the earlier system-reminder have changed since it was written. Below is a unified diff against that listing: `-` lines are skills that are gone — they are uninstalled, and calling one will fail — and `+` lines are available now. If this conversation carries more than one such update, the last one wins."#;

/// Body for a listing that has come back into agreement — a skill uninstalled
/// and reinstalled, or a reload that changed nothing this session can see.
/// Nothing differs any more, so there is no diff to send; what is needed is a
/// retraction, because an earlier update is still asserting a state that has
/// since been undone and the model has no other way to learn that.
const NOTHING_DIFFERS_BODY: &str = r#"Disregard any earlier skills update in this conversation: the skills listed in the earlier system-reminder are available again exactly as listed there."#;

/// Wrap the difference between the listing the session was shown and the one it
/// would be shown now.
///
/// `None` when the two agree and there is nothing to retract — the common case,
/// and the only one that must stay silent. When an update is already standing,
/// agreement is not silence: it means the registry moved and moved back, and
/// the standing update has to be retracted, which is what `told_before` selects.
///
/// Always a diff, at any size. A full listing communicates a removal only by
/// absence, and the whole point of the notice is that the model stops planning
/// around a skill it can no longer call — so the size a wholesale-replacement
/// shortcut would save is declined on purpose.
pub fn wrap_update(seeded: &str, current: &str, told_before: bool) -> Option<String> {
    let diff = unified_diff(seeded, current);
    if diff.is_empty() {
        return told_before.then(|| NOTHING_DIFFERS_BODY.to_string());
    }
    Some(format!(
        "{FRAMING_BODY}\n\n{OPEN_TAG}\n<{SKILLS_TAG} {DIFF_ATTR}>\n{diff}</{SKILLS_TAG}>\n{CLOSE_TAG}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: &str = "- alpha: does alpha things\n- beta: does beta things";

    #[test]
    fn an_added_skill_is_the_only_line_sent() {
        let out = wrap_update(TWO, &format!("{TWO}\n- gamma: does gamma things"), false)
            .expect("an update");
        assert!(out.contains("+- gamma: does gamma things"), "{out}");
        assert!(
            !out.contains("+- alpha"),
            "unchanged skills ride as context: {out}"
        );
        assert!(
            out.contains(&format!("<{SKILLS_TAG} {DIFF_ATTR}>")),
            "{out}"
        );
    }

    /// The case the notice exists for. A removal has to be stated, never
    /// implied by a shorter list.
    #[test]
    fn a_removed_skill_is_stated_outright() {
        let out = wrap_update(TWO, "- alpha: does alpha things", false).expect("an update");
        assert!(out.contains("-- beta: does beta things"), "{out}");
        assert!(
            out.contains("uninstalled"),
            "the consequence must be named: {out}"
        );
    }

    #[test]
    fn an_edited_description_sends_both_sides() {
        let out = wrap_update(
            TWO,
            &TWO.replace("does beta things", "does BETTER things"),
            false,
        )
        .expect("an update");
        assert!(out.contains("-- beta: does beta things"), "{out}");
        assert!(out.contains("+- beta: does BETTER things"), "{out}");
    }

    /// A registry that moved without changing anything THIS session can see —
    /// another agent's overlay, a channel-restricted skill — must cost nothing.
    #[test]
    fn a_change_this_session_cannot_see_says_nothing() {
        assert!(wrap_update(TWO, TWO, false).is_none());
    }

    /// …but once an update is standing, agreement is a retraction rather than
    /// silence: the standing one still asserts a state that has been undone.
    #[test]
    fn agreement_retracts_a_standing_update() {
        let out = wrap_update(TWO, TWO, true).expect("a retraction");
        assert!(out.contains("Disregard any earlier skills update"), "{out}");
        assert!(!out.contains(OPEN_TAG), "nothing to wrap: {out}");
    }

    /// A session seeded with no skills has no listing row, so its baseline is
    /// empty — and the first skill installed must still arrive.
    #[test]
    fn a_first_skill_against_an_empty_baseline_arrives() {
        let out = wrap_update("", "- alpha: does alpha things", false).expect("an update");
        assert!(out.contains("+- alpha: does alpha things"), "{out}");
        assert!(
            !out.lines().any(|l| l == "-"),
            "no phantom removed line: {out}"
        );
    }

    /// The inverse: the last skill uninstalled empties the listing, and that
    /// must not render as "one blank skill".
    #[test]
    fn removing_the_last_skill_leaves_no_phantom_line() {
        let out = wrap_update("- alpha: does alpha things", "", false).expect("an update");
        assert!(out.contains("-- alpha: does alpha things"), "{out}");
        assert!(
            !out.lines().any(|l| l == "+"),
            "no phantom added line: {out}"
        );
    }
}
