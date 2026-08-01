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
}
