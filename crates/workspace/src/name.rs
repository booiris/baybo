//! Reading and rewriting the `Name:` line of an `IDENTITY.md` body.
//!
//! Pure string work, deliberately outside the `io` feature: the `Edit` tool
//! guards this field and takes this crate without `io`.

/// Pull the agent's chosen name out of an `IDENTITY.md` body.
///
/// The file is prose the agent rewrites freely, so this is a tolerant scan,
/// not a parser: the first line carrying a `Name:` label wins, whatever
/// bullet or emphasis surrounds it, and the value is whatever follows on
/// that line. `None` when there is no such line or its value is empty —
/// which is the shipped template's state, since it invites the agent to
/// choose. Callers supply their own fallback rather than getting a
/// placeholder baked in here.
pub fn display_name(identity_md: &str) -> Option<String> {
    identity_md.lines().find_map(|line| {
        let (label, value) = line.split_once(':')?;
        if !label_is_name(label) {
            return None;
        }
        let value = strip_emphasis(value);
        (!value.is_empty()).then(|| value.to_owned())
    })
}

/// Rewrite (or introduce) the `Name:` line in an `IDENTITY.md` body.
///
/// Preserves everything else verbatim — this is a targeted edit to a file
/// the agent owns, so it must not reformat the parts it was not asked to
/// touch. When no `Name:` line exists the entry is inserted after the
/// leading heading, where the template puts it.
pub fn with_display_name(identity_md: &str, name: &str) -> String {
    // A name is one line by construction; anything else would break the very
    // line this function keys off.
    let name = name.split(['\n', '\r']).next().unwrap_or_default().trim();
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in identity_md.lines() {
        if !replaced
            && let Some((label, rest)) = line.split_once(':')
            && label_is_name(label)
        {
            // Splice the value only. The emphasis run right after the colon
            // is the *closing* half of the label's bold (`* **Name:**`), so
            // rebuilding the line from the label alone would eat it.
            let closing = &rest[..rest.len() - rest.trim_start_matches(['*', '_']).len()];
            out.push(format!("{label}:{closing} {name}"));
            replaced = true;
            continue;
        }
        out.push(line.to_owned());
    }
    if !replaced {
        let after_heading = out
            .iter()
            .position(|l| l.trim_start().starts_with('#'))
            .map_or(0, |i| i + 1);
        let line = format!("* **Name:** {name}");
        // The blank line separates the entry from the heading above it; with
        // no heading there is nothing to separate it from.
        if after_heading > 0 {
            out.insert(after_heading, String::new());
            out.insert(after_heading + 1, line);
        } else {
            out.insert(0, line);
        }
    }
    let mut joined = out.join("\n");
    if identity_md.ends_with('\n') || joined.is_empty() {
        joined.push('\n');
    }
    joined
}

/// Whether this agent's display name is settled for good once it has one.
///
/// A project agent is hired *under* a name: the `@handle` its board addresses
/// it by is derived from that name at creation and then never moves, so a
/// later rename would leave the roster calling it one thing while every
/// mention, assignment and timeline entry says another. A global chat persona
/// answers to no handle and stays free to rename itself.
///
/// Keyed on the persona id — the same prefix that routes the agent's files —
/// because the answer has to be available wherever an id is, including the
/// tool layer, which has no store to ask.
pub fn name_is_fixed(agent_id: &str) -> bool {
    crate::paths::is_project_persona_id(agent_id)
}

/// Why rewriting `agent_id`'s `IDENTITY.md` body from `before` into `after`
/// renames it when it may not be renamed, or `None` when the change is fine.
///
/// **Every** writer of that file asks this one — the agent's own `Edit` and
/// `Write`, the operator's `PUT /v1/agents/{id}/name` and `…/identity` —
/// because a rule enforced at three of four doors is not a rule.
///
/// A file that carries no name yet is never held hostage: naming it is a
/// repair, not a rename, and with the rename refused it is the only way back
/// from an `IDENTITY.md` that was deleted and re-seeded from the template.
pub fn rejected_rename(agent_id: &str, before: &str, after: &str) -> Option<String> {
    if !name_is_fixed(agent_id) {
        return None;
    }
    let current = display_name(before)?;
    if display_name(after).as_deref() == Some(current.as_str()) {
        return None;
    }
    Some(format!(
        "this agent's name is fixed at {current:?}: it works a project board, and the @handle \
         its teammates address it by was derived from that name when it was created. Renaming \
         it would leave the two disagreeing about who this is."
    ))
}

/// Why rewriting an `IDENTITY.md` from `before` into `after` would leave it
/// with no readable `Name:` line, or `None` when it would not.
///
/// Narrower reach than [`rejected_rename`] on purpose: this guards the *tool*
/// doors, where the writer is a model editing one field and an incidental
/// reformat could cost the agent its name without failing anything loudly.
/// An operator replacing the whole document through the API means what they
/// typed — including restoring the shipped template, which carries no name —
/// and an unnamed agent renders as its id rather than as nothing.
pub fn rejected_name_removal(before: &str, after: &str) -> Option<String> {
    let orphaned = display_name(before).is_some() && display_name(after).is_none();
    orphaned.then(|| NAME_REMOVAL_REFUSAL.to_owned())
}

const NAME_REMOVAL_REFUSAL: &str = "this would remove the `Name:` line from IDENTITY.md, which is what every surface calls \
     this agent — without it, it renders as its raw id. Keep a `Name: <something>` line.";

/// Whether the text before a `:` is the name label, ignoring the markdown
/// decoration the template ships with (`* **Name:**`) and any casing.
fn label_is_name(label: &str) -> bool {
    strip_emphasis(label).eq_ignore_ascii_case("name")
}

/// Strip list bullets and `*` / `_` emphasis from around a fragment.
fn strip_emphasis(fragment: &str) -> &str {
    fragment
        .trim()
        .trim_start_matches(['-', '*', '+'])
        .trim()
        .trim_matches(['*', '_'])
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::IdentityKind;

    #[test]
    fn display_name_reads_the_shipped_template_shape_and_tolerates_drift() {
        // What the template produces once filled in.
        assert_eq!(
            display_name("# Who Am I?\n\n* **Name:** Aster\n* **Vibe:** dry\n").as_deref(),
            Some("Aster")
        );
        // The agent owns this file, so the scan must survive it reformatting.
        for drifted in [
            "Name: Aster",
            "- name: Aster",
            "  * **NAME:**   Aster  ",
            "## Who\n\n**Name**: Aster\n",
        ] {
            assert_eq!(
                display_name(drifted).as_deref(),
                Some("Aster"),
                "failed on {drifted:?}"
            );
        }
        // The shipped template is deliberately unnamed — it invites the agent
        // to choose — so callers must supply their own fallback.
        assert_eq!(display_name(IdentityKind::Identity.default_content()), None);
        assert_eq!(display_name("no labels here"), None);
        assert_eq!(display_name("* **Name:**"), None);
    }

    #[test]
    fn with_display_name_edits_only_the_name_line() {
        let original = "# Who Am I?\n\n* **Name:** Aster\n* **Vibe:** dry\n";
        let renamed = with_display_name(original, "Vega");
        assert_eq!(
            renamed,
            "# Who Am I?\n\n* **Name:** Vega\n* **Vibe:** dry\n"
        );
        assert_eq!(display_name(&renamed).as_deref(), Some("Vega"));

        // Round-trips: naming an unnamed template makes it readable, and
        // everything the agent wrote around it survives.
        let seeded = with_display_name(IdentityKind::Identity.default_content(), "Vega");
        assert_eq!(display_name(&seeded).as_deref(), Some("Vega"));
        assert!(seeded.contains("**Creature:**"), "{seeded}");

        // A file with no name line at all gains one under the heading.
        let added = with_display_name("# Who Am I?\n\nfree prose\n", "Vega");
        assert_eq!(display_name(&added).as_deref(), Some("Vega"));
        assert!(added.contains("free prose"), "{added}");

        // A multi-line value would destroy the line this keys off.
        let sneaky = with_display_name(original, "Vega\n* **Vibe:** hijacked");
        assert_eq!(display_name(&sneaky).as_deref(), Some("Vega"));
        assert!(sneaky.contains("* **Vibe:** dry"), "{sneaky}");
    }

    const GLOBAL: &str = "01JAGENT";
    const ON_A_BOARD: &str = "project-01JAGENT";

    #[test]
    fn only_a_project_agents_name_is_fixed() {
        assert!(name_is_fixed(ON_A_BOARD));
        assert!(!name_is_fixed(GLOBAL));
        assert!(!name_is_fixed(crate::paths::BUILTIN_PERSONA_DIR));
    }

    #[test]
    fn a_project_agent_cannot_rename_itself_and_a_global_one_can() {
        let named = "# Who Am I?\n\n* **Name:** Lead\n* **Vibe:** dry\n";
        let renamed = with_display_name(named, "Aster");

        let refusal = rejected_rename(ON_A_BOARD, named, &renamed).expect("refused");
        assert!(refusal.contains("@handle"), "{refusal}");
        assert!(refusal.contains("\"Lead\""), "{refusal}");
        assert_eq!(rejected_rename(GLOBAL, named, &renamed), None);

        // Rewriting everything *but* the name is what these files are for.
        let rewritten = named.replace("dry", "warm");
        assert_eq!(rejected_rename(ON_A_BOARD, named, &rewritten), None);

        // Dropping the line is a rename to nothing, not a way around it.
        assert!(rejected_rename(ON_A_BOARD, named, "# Who Am I?\n").is_some());
    }

    #[test]
    fn naming_a_file_that_has_no_name_is_a_repair_not_a_rename() {
        // The recovery path after an IDENTITY.md is deleted and re-seeded
        // from the template: with the rename refused, this is the only door
        // left, so it must stay open even for a fixed name.
        let template = IdentityKind::Identity.default_content();
        let named = with_display_name(template, "Lead");
        for agent in [GLOBAL, ON_A_BOARD] {
            assert_eq!(rejected_rename(agent, template, &named), None);
            assert_eq!(rejected_rename(agent, "", &named), None);
        }
    }

    #[test]
    fn losing_the_name_line_is_refused_whoever_the_agent_is() {
        let named = "# Who Am I?\n\n* **Name:** Lead\n";
        let refusal = rejected_name_removal(named, "# Who Am I?\n").expect("refused");
        assert!(refusal.contains("Name: <something>"), "{refusal}");
        // A file that never had one is not held hostage, and a rename is
        // somebody else's rule.
        assert_eq!(
            rejected_name_removal("# Who Am I?\n", "still nothing"),
            None
        );
        assert_eq!(
            rejected_name_removal(named, &with_display_name(named, "Aster")),
            None
        );
    }
}
