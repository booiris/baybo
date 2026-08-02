//! Framing for the notice that reconciles a stale leading `Role::System` row
//! with the sources it was assembled from.
//!
//! The system row is written once, at seed, and persisted. A session outlives
//! the deploy that created it, so a release that moves the persona files — or a
//! plain edit to `SOUL.md` — leaves the model reading a prompt that names paths
//! it can no longer write to. Replacing the row in place would be the obvious
//! fix and is the wrong one mid-session: `messages[0]` is the head of the
//! prompt-cache prefix, so rewriting it invalidates the cached transcript on
//! every provider. Appending the delta at the tail costs only the changed parts;
//! [`crate::ContextManager`]'s post-compaction reseed is what retires the row
//! itself, and it drops these along with it.
//!
//! The delta is expressed in the units [`crate::prompts::soul::AssembledPrompt`]
//! hands over — a hint block, or one wrapped `<tag path="…">` identity section —
//! so a moved path or an edited file reports as the one part that carries it
//! rather than as a whole new prompt.
//!
//! **Updates accumulate, and that is the cheap option.** Each is a complete
//! delta against the leading row, so only the newest carries information — but
//! removing an older one from the request rewrites it at whatever
//! mid-transcript position that row occupies, which invalidates the provider's
//! cached prefix from there on and re-bills everything after it, on every
//! subsequent call. Appending at the tail is the only edit a cached prefix
//! tolerates. Superseded blocks are therefore left where they are and the
//! framing tells the model the last one wins; the size that buys is bounded by
//! keeping each delta to the parts that actually moved.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::prompts::soul::{PromptPart, SectionTag};

const OPEN_TAG: &str = "<system_prompt_update>";
const CLOSE_TAG: &str = "</system_prompt_update>";

/// Framing preamble placed before the tagged block.
///
/// Three claims in it are load-bearing rather than decorative.
///
/// **Every claim is scoped per block.** The payload is a delta — only the parts
/// that moved — so a blanket "treat every path in the system prompt as out of
/// date" would be false about the majority of them, and would leave a model that
/// obeyed it with nowhere to write the sections the update does not mention.
/// Hence the explicit converse: what is not restated below is unchanged.
///
/// **A superseded path is still writable.** The migration this exists for left
/// the old `profile/` directory on disk, so an `Edit` against the path an old
/// system prompt names succeeds, writes real bytes, and is never read again —
/// telling the model the path merely "changed" is not enough to stop that, so
/// the preamble says what happens if it tries.
///
/// **Latest wins.** Updates accumulate until a compaction retires them, and each
/// restates the whole delta against the original prompt, so without that rule a
/// sequence of them reads as contradictory.
const FRAMING_BODY: &str = r#"The system prompt at the top of this conversation was assembled when the conversation started, and some of the files behind it have moved or changed since. Each block below REPLACES the part of that system prompt carrying the same tag. Any part of that system prompt not restated below is unchanged and still current.

Where a block below names a `path`, that is where the file lives now. If the system prompt above gave a different path for that same block, the old one is dead — and it may well still exist on disk, so writing to it will appear to succeed and nothing will ever read what you wrote.

If this conversation carries more than one such update, the last one wins."#;

/// Body used when the sources have come back into agreement with the leading
/// system row — a source that moved and then moved back. Nothing differs any
/// more, so there is no block to send; what is needed is a retraction, because
/// an earlier update in this conversation is still asserting a state that has
/// since been undone and the model has no other way to learn that.
const NOTHING_DIFFERS_BODY: &str = r#"Disregard any earlier system-prompt update in this conversation. The files behind the system prompt have changed back: the system prompt at the top of this conversation is accurate again, and every path it names is current."#;

/// Attribute marking a section the model rewrote itself during this
/// conversation. Named once: the renderer writes it and [`SELF_EDIT_FRAMING`]
/// explains it, and the two have to agree.
const SELF_EDITED_ATTR: &str = "edited_by_you_in_this_conversation";

/// Appended to [`FRAMING_BODY`] only when the update actually carries a
/// self-edited pointer, so an update without one is byte-identical to what
/// shipped before this existed.
///
/// Deliberately narrow about what is stale. The model changed part of a file;
/// the rest of the body in the system prompt above is still accurate, and
/// telling it otherwise would strand it with no usable copy of a persona it can
/// still mostly trust. It is pointed at `Read` for the exact bytes rather than
/// handed them, because it has just written them.
const SELF_EDIT_FRAMING: &str = r#"One or more blocks below are empty and carry `edited_by_you_in_this_conversation`. Those are not replacements: you rewrote that file yourself earlier in this conversation, so the copy in the system prompt above is out of date exactly where you changed it and still accurate everywhere else. Your own edit is what the file says now. `Read` the path if you need its precise current content."#;

/// One entry of a rendered update.
pub enum UpdateBlock<'a> {
    /// The part's current text, replacing the same part above.
    Full(&'a str),
    /// A section the model rewrote itself: named, not restated.
    SelfEdited { tag: SectionTag, path: &'a Path },
}

impl UpdateBlock<'_> {
    fn render(&self) -> String {
        match self {
            UpdateBlock::Full(text) => (*text).to_string(),
            UpdateBlock::SelfEdited { tag, path } => format!(
                "<{tag} path=\"{path}\" {SELF_EDITED_ATTR}/>",
                tag = tag.as_str(),
                path = path.display(),
            ),
        }
    }
}

/// The parts of a freshly assembled prompt that the leading `Role::System` row
/// does not already carry — a **complete** delta against that row, deliberately
/// not against any update appended since.
///
/// That is what makes each update supersede every earlier one instead of
/// building on it, which is the property [`wrap_update`]'s "the last one wins"
/// rule depends on. `seeded` is therefore the leading row's text alone.
///
/// A part counts as already carried when its rendered text appears in `seeded`
/// verbatim, which is exactly the test that matters: parts are multi-line blocks
/// joined into the prompt unchanged, so a substring hit means the model is
/// looking at those bytes and a miss means it is not. Order follows `current`,
/// so a rendered update reads top-down like the prompt it patches.
///
/// A section whose file is in `self_edited` degrades to a pointer instead of its
/// body — the model wrote those bytes, so restating them costs the whole file to
/// tell it what it already did. Sections only: a hint has no file behind it, and
/// a body absent from `seeded` for any *other* reason is still sent in full.
pub fn build_blocks<'a>(
    current: &'a [PromptPart],
    rendered: &'a [String],
    seeded: &str,
    self_edited: &HashSet<PathBuf>,
) -> Vec<UpdateBlock<'a>> {
    current
        .iter()
        .zip(rendered)
        .filter(|(_, text)| !seeded.contains(text.as_str()))
        .map(|(part, text)| match part {
            PromptPart::Section { tag, path, .. } if self_edited.contains(path) => {
                UpdateBlock::SelfEdited { tag: *tag, path }
            }
            _ => UpdateBlock::Full(text.as_str()),
        })
        .collect()
}

/// Wrap the changed blocks in the `<system_prompt_update>` envelope. An empty
/// `blocks` renders the retraction instead — see [`NOTHING_DIFFERS_BODY`].
///
/// Like [`crate::prompts::recalled_memory`], the body is not breakout-escaped:
/// every byte here was produced by the prompt assembly this process just ran,
/// out of the workspace's own identity files. A workspace whose `SOUL.md` is
/// hostile has already won by being the system prompt.
pub fn wrap_update(blocks: &[UpdateBlock<'_>]) -> String {
    if blocks.is_empty() {
        return NOTHING_DIFFERS_BODY.to_string();
    }
    let joined = blocks
        .iter()
        .map(UpdateBlock::render)
        .collect::<Vec<_>>()
        .join("\n\n");
    let framing = if blocks
        .iter()
        .any(|b| matches!(b, UpdateBlock::SelfEdited { .. }))
    {
        format!("{FRAMING_BODY}\n\n{SELF_EDIT_FRAMING}")
    } else {
        FRAMING_BODY.to_string()
    };
    format!("{framing}\n\n{OPEN_TAG}\n{joined}\n{CLOSE_TAG}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(text: &str) -> PromptPart {
        PromptPart::Hint(text.to_string())
    }

    fn soul(path: &str, body: &str) -> PromptPart {
        PromptPart::Section {
            tag: SectionTag::Soul,
            path: PathBuf::from(path),
            body: body.to_string(),
        }
    }

    /// Render each part the way the assembly does, so the substring test in
    /// `build_blocks` sees exactly what the prompt carries.
    fn blocks<'a>(
        parts: &'a [PromptPart],
        rendered: &'a [String],
        seeded: &str,
        self_edited: &HashSet<PathBuf>,
    ) -> Vec<UpdateBlock<'a>> {
        build_blocks(parts, rendered, seeded, self_edited)
    }

    fn render_all(parts: &[PromptPart]) -> Vec<String> {
        parts.iter().map(PromptPart::render).collect()
    }

    #[test]
    fn a_part_already_in_the_system_row_is_not_reported() {
        let parts = vec![soul("/new", "body")];
        let rendered = render_all(&parts);
        let seeded = format!("preamble\n\n{}\n\ntail", rendered[0]);
        assert!(blocks(&parts, &rendered, &seeded, &HashSet::new()).is_empty());
    }

    #[test]
    fn a_moved_path_reports_only_the_section_that_moved() {
        let parts = vec![hint("top hint"), soul("/new/SOUL.md", "body"), hint("tail")];
        let rendered = render_all(&parts);
        let seeded = "top hint\n\n<soul path=\"/old/SOUL.md\">\nbody\n</soul>\n\ntail";
        let out = wrap_update(&blocks(&parts, &rendered, seeded, &HashSet::new()));
        assert!(out.contains("/new/SOUL.md"), "{out}");
        assert!(!out.contains("top hint") && !out.contains("/old/"), "{out}");
    }

    /// The delta is measured against the leading row ONLY. A part an earlier
    /// update already carried is reported again, on purpose: that is what keeps
    /// every update a complete delta, so the newest supersedes the rest.
    #[test]
    fn the_delta_is_measured_against_the_leading_row_not_against_earlier_updates() {
        let parts = vec![soul("/new", "v2")];
        let rendered = render_all(&parts);
        let out = wrap_update(&blocks(
            &parts,
            &rendered,
            "<soul path=\"/old\">\nv1\n</soul>",
            &HashSet::new(),
        ));
        assert!(out.contains("v2"), "{out}");
    }

    #[test]
    fn an_empty_delta_renders_a_retraction_rather_than_an_empty_envelope() {
        let out = wrap_update(&[]);
        assert!(!out.contains(OPEN_TAG), "nothing to wrap: {out}");
        assert!(
            out.contains("Disregard any earlier system-prompt update"),
            "{out}"
        );
    }

    #[test]
    fn parts_keep_prompt_order_inside_one_envelope() {
        let parts = vec![hint("a"), hint("b")];
        let rendered = render_all(&parts);
        let out = wrap_update(&blocks(&parts, &rendered, "nothing here", &HashSet::new()));
        assert_eq!(out.matches(OPEN_TAG).count(), 1);
        assert!(out.contains("REPLACE"));
        assert!(out.contains(&format!("{OPEN_TAG}\na\n\nb\n{CLOSE_TAG}")));
    }

    /// A section the model rewrote itself is named, not restated — and the
    /// extra framing paragraph only appears when one is actually present.
    #[test]
    fn a_self_edited_section_is_pointed_at_rather_than_repeated() {
        let parts = vec![soul("/w/SOUL.md", "A VERY LONG BODY THE MODEL JUST WROTE")];
        let rendered = render_all(&parts);
        let mine = HashSet::from([PathBuf::from("/w/SOUL.md")]);
        let out = wrap_update(&blocks(&parts, &rendered, "stale row", &mine));

        assert!(out.contains(SELF_EDITED_ATTR), "{out}");
        assert!(out.contains("<soul path=\"/w/SOUL.md\""), "{out}");
        assert!(
            !out.contains("A VERY LONG BODY"),
            "the body the model wrote must not be echoed back: {out}"
        );
        assert!(out.contains("edited_by_you_in_this_conversation"), "{out}");
    }

    #[test]
    fn an_update_without_a_self_edit_carries_no_extra_framing() {
        let parts = vec![soul("/w/SOUL.md", "body")];
        let rendered = render_all(&parts);
        let out = wrap_update(&blocks(&parts, &rendered, "stale row", &HashSet::new()));
        assert!(!out.contains(SELF_EDITED_ATTR), "{out}");
        assert_eq!(
            out,
            format!("{FRAMING_BODY}\n\n{OPEN_TAG}\n{}\n{CLOSE_TAG}", rendered[0])
        );
    }

    /// A hint has no file behind it, so it can never be elided even if some
    /// path in `self_edited` happens to be unrelated.
    #[test]
    fn a_hint_is_never_elided_as_self_edited() {
        let parts = vec![hint("# Memory\n\nrules")];
        let rendered = render_all(&parts);
        let mine = HashSet::from([PathBuf::from("/w/SOUL.md")]);
        let out = wrap_update(&blocks(&parts, &rendered, "stale row", &mine));
        assert!(out.contains("rules"), "{out}");
        assert!(!out.contains(SELF_EDITED_ATTR), "{out}");
    }
}
