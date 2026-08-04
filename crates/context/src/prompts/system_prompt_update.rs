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
//! **A changed section is sent as a line diff, not as its new body.** The
//! sections are addressed by tag, so the copy the leading row carries can be
//! recovered from that row and diffed against the live one — a session that
//! appended one line to `MEMORY.md` then sends that line rather than the whole
//! index, and a `personas/` directory that moved sends a self-closing tag
//! carrying the new path. Full text is the fallback, for the cases where a diff
//! cannot be formed or would not be smaller: a hint (no tag, so no prior copy to
//! diff against), a section the prompt did not carry before, a rewrite.
//!
//! **A diff is measured against the newest copy the conversation is holding**,
//! which is the leading row's only until the model rewrites the file itself.
//! A whole-file `Write` puts the new body in the transcript verbatim, as the
//! call's own parameter, and [`SELF_EDITED_ATTR`] then elides it from the
//! update — so from that point the row's copy is stale by an entire rewrite and
//! nothing else has replaced it. Diffing the next change by somebody else
//! against the row would quote both copies of the file and lose to sending it
//! whole; against the model's own write it is the lines that actually moved,
//! and it says the thing that matters, which is that somebody else has been in
//! the file. An `Edit` leaves no such copy — it names the fragment it replaced,
//! not the result — so those sections keep the leading row as their baseline,
//! which an edit leaves only slightly stale.
//!
//! **Updates accumulate, and that is the cheap option.** Each is a complete
//! delta against the leading row, so only the newest carries information — but
//! removing an older one from the request rewrites it at whatever
//! mid-transcript position that row occupies, which invalidates the provider's
//! cached prefix from there on and re-bills everything after it, on every
//! subsequent call. Appending at the tail is the only edit a cached prefix
//! tolerates. Superseded blocks are therefore left where they are and the
//! framing tells the model the last one wins; the size that buys is bounded by
//! keeping each delta to the lines that actually moved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::prompts::line_diff::{DIFF_ATTR, unified_diff};
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
/// Hence the explicit converse: what is not named below is unchanged.
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
const FRAMING_BODY: &str = r#"The system prompt above was assembled when this conversation started; some of its sources have changed since. Each block below updates the part carrying the same tag — anything not named is still current, and the last such update wins.

A block's `path` is where that file lives now. Any other path the prompt above gives for that tag is dead: it may still exist on disk, so writing there appears to succeed and is never read."#;

/// Appended to [`FRAMING_BODY`] only when a [`UpdateBlock::Diff`] is actually
/// present, so an update made only of full bodies is unchanged by this
/// existing.
///
/// Both halves say something the model cannot get anywhere else. Which copy
/// the diff is against is a fact about this system, not about diffs. And an
/// *unmarked* block being a whole-part replacement is the one thing the
/// attribute alone cannot state — without it, a full body reads as a diff that
/// happens to carry no markers.
///
/// What `-` and `+` mean is deliberately absent: unified diff is not a format
/// a model has to be taught, and glossing it is the one part of this paragraph
/// that every request would pay for and none would use.
const DIFF_FRAMING: &str = r#"A block marked `diff` is a unified diff against the copy in the system prompt above. An unmarked block is that part's full current text."#;

/// Attribute marking a diff whose baseline is the copy the model wrote itself
/// rather than the one the leading row carries. Rides alongside [`DIFF_ATTR`],
/// which says the body is a diff at all; this says which copy it is against.
const SINCE_YOUR_WRITE_ATTR: &str = "changed_since_you_wrote_it";

/// Appended to [`FRAMING_BODY`] only when a block is diffed against the model's
/// own write, so an update without one is unchanged by this existing.
///
/// It has to name the baseline, because "the copy above" stops being
/// unambiguous the moment the conversation holds two of them — the system
/// prompt's and the model's own tool call. It also states the fact the block
/// exists to report: the model's write stood, and then somebody else wrote over
/// part of it. It says neither of the things [`DIFF_FRAMING`] says, and does
/// not need that paragraph present: which copy is the baseline is exactly what
/// it is here to state, and a block reaching this variant is never unmarked.
const SINCE_YOUR_WRITE_FRAMING: &str = r#"A block marked `changed_since_you_wrote_it` is a unified diff against the copy YOU wrote earlier in this conversation, not the older one in the system prompt above. Something else has written to that file since."#;

/// Body used when the sources have come back into agreement with the leading
/// system row — a source that moved and then moved back. Nothing differs any
/// more, so there is no block to send; what is needed is a retraction, because
/// an earlier update in this conversation is still asserting a state that has
/// since been undone and the model has no other way to learn that.
const NOTHING_DIFFERS_BODY: &str = r#"Disregard any earlier system-prompt update in this conversation: the files behind the system prompt have changed back, so the prompt above is accurate again and every path it names is current."#;

/// Attribute marking a section the model rewrote itself during this
/// conversation, and that still holds those bytes. Named once: the renderer
/// writes it, [`SELF_EDIT_FRAMING`] explains it, and
/// `ContextManager::self_written_sections` decides when it is true — all three
/// have to agree.
pub(crate) const SELF_EDITED_ATTR: &str = "edited_by_you_in_this_conversation";

/// Attribute marking a section whose file moved with its contents intact.
/// Self-describing on purpose — the framing already says what `path` means, so
/// this case costs an attribute rather than a paragraph.
const MOVED_ATTR: &str = "content_unchanged";

/// Appended to [`FRAMING_BODY`] only when the update actually carries a
/// self-edited pointer, so an update without one is byte-identical to what
/// shipped before this existed.
///
/// Deliberately narrow about what is stale. The model changed part of a file;
/// the rest of the body in the system prompt above is still accurate, and
/// telling it otherwise would strand it with no usable copy of a persona it can
/// still mostly trust. It is pointed at `Read` for the exact bytes rather than
/// handed them, because it has just written them.
const SELF_EDIT_FRAMING: &str = r#"An empty block marked `edited_by_you_in_this_conversation` is not a replacement: you rewrote that file yourself earlier in this conversation, so the copy above is out of date exactly where you changed it and accurate everywhere else. Your own edit is what it says now; `Read` the path for the exact bytes."#;

/// What this conversation last wrote to one file: the copy of it the model is
/// holding, which is newer than the one its system prompt carries.
///
/// Borrowed from the transcript rather than owned. The reconciler runs before
/// every LLM call, and a conversation that wrote a large file would otherwise
/// copy it out of the message list on each one.
pub struct SelfWrite<'a> {
    /// The file still holds exactly those bytes, so the section needs no body
    /// at all — the model produced the current contents itself.
    pub still_current: bool,
    /// The whole body the call carried, for a `Write` — the baseline a later
    /// change by somebody else is diffed against. `None` for an `Edit`, which
    /// names the fragment it replaced rather than the result, so the leading
    /// row stays the only copy that section can be diffed against.
    pub body: Option<&'a str>,
}

/// One entry of a rendered update.
pub enum UpdateBlock<'a> {
    /// The part's current text, replacing the same part above. The fallback:
    /// a hint, a section the leading row never carried, or one whose diff came
    /// out no smaller than the body it describes.
    Full(&'a str),
    /// A unified diff against the copy the leading row carries.
    Diff {
        tag: SectionTag,
        path: &'a Path,
        diff: String,
    },
    /// A unified diff against the copy the model wrote itself, for a file
    /// somebody else has written to since.
    SinceYourWrite {
        tag: SectionTag,
        path: &'a Path,
        diff: String,
    },
    /// The same bytes at a new path.
    Moved { tag: SectionTag, path: &'a Path },
    /// A section the model rewrote itself: named, not restated.
    SelfEdited { tag: SectionTag, path: &'a Path },
}

impl UpdateBlock<'_> {
    fn render(&self) -> String {
        match self {
            UpdateBlock::Full(text) => (*text).to_string(),
            UpdateBlock::Diff { tag, path, diff } => {
                diff_block(*tag, path, diff, DIFF_ATTR.to_string())
            }
            UpdateBlock::SinceYourWrite { tag, path, diff } => diff_block(
                *tag,
                path,
                diff,
                format!("{DIFF_ATTR} {SINCE_YOUR_WRITE_ATTR}"),
            ),
            UpdateBlock::Moved { tag, path } => self_closing(*tag, path, MOVED_ATTR),
            UpdateBlock::SelfEdited { tag, path } => self_closing(*tag, path, SELF_EDITED_ATTR),
        }
    }
}

/// `unified_diff` terminates its output, so the closing tag follows the body
/// directly — an extra newline here would render as a trailing blank line.
fn diff_block(tag: SectionTag, path: &Path, diff: &str, attrs: String) -> String {
    format!(
        "<{tag} path=\"{path}\" {attrs}>\n{diff}</{tag}>",
        tag = tag.as_str(),
        path = path.display(),
    )
}

fn self_closing(tag: SectionTag, path: &Path, attr: &str) -> String {
    format!(
        "<{tag} path=\"{path}\" {attr}/>",
        tag = tag.as_str(),
        path = path.display(),
    )
}

/// The parts of a freshly assembled prompt that the leading `Role::System` row
/// does not already carry — a **complete** delta against that row, deliberately
/// not against any update appended since.
///
/// That is what makes each update supersede every earlier one instead of
/// building on it, which is the property [`wrap_update`]'s "the last one wins"
/// rule depends on. `seeded` is therefore the leading row's text alone, and it
/// is also where a changed section's *prior* copy is read back from, so the
/// diff a block carries is likewise measured against that row and nothing else.
///
/// A part counts as already carried when its rendered text appears in `seeded`
/// verbatim, which is exactly the test that matters: parts are multi-line blocks
/// joined into the prompt unchanged, so a substring hit means the model is
/// looking at those bytes and a miss means it is not. Order follows `current`,
/// so a rendered update reads top-down like the prompt it patches.
///
/// A section this conversation wrote itself is described against its own write
/// rather than against `seeded`: as a pointer while the file still holds those
/// bytes — restating them costs the whole file to tell the model what it
/// already did — and as a diff from them once somebody else has written over
/// part of it. Sections only: a hint has no file behind it, and a body absent
/// from `seeded` for any *other* reason is still described in full or by diff.
pub fn build_blocks<'a>(
    current: &'a [PromptPart],
    rendered: &'a [String],
    seeded: &str,
    self_written: &HashMap<PathBuf, SelfWrite<'_>>,
) -> Vec<UpdateBlock<'a>> {
    current
        .iter()
        .zip(rendered)
        .filter(|(_, text)| !seeded.contains(text.as_str()))
        .map(|(part, text)| block_for(part, text, seeded, self_written))
        .collect()
}

/// Pick the cheapest honest description of one changed part.
fn block_for<'a>(
    part: &'a PromptPart,
    text: &'a str,
    seeded: &str,
    self_written: &HashMap<PathBuf, SelfWrite<'_>>,
) -> UpdateBlock<'a> {
    // A hint carries no tag, so the leading row offers nothing to diff it
    // against; it is opaque framing text that only a binary upgrade moves.
    let PromptPart::Section { tag, path, body } = part else {
        return UpdateBlock::Full(text);
    };
    let body = body.trim_end_matches('\n');
    if let Some(mine) = self_written.get(path)
        && let Some(block) = block_against_own_write(mine, *tag, path, body, text)
    {
        return block;
    }
    let Some(prior) = seeded_section_body(seeded, *tag) else {
        return UpdateBlock::Full(text);
    };
    if prior == body {
        return UpdateBlock::Moved { tag: *tag, path };
    }
    // A plain diff and the full body say exactly the same thing, so the choice
    // between them is a choice of size alone and the paragraph the first diff
    // summons is part of what this one costs.
    match worthwhile_diff(prior, body, text, DIFF_FRAMING.len()) {
        Some(diff) => UpdateBlock::Diff {
            tag: *tag,
            path,
            diff,
        },
        None => UpdateBlock::Full(text),
    }
}

/// The block for a section this conversation wrote itself, or `None` when its
/// own copy is no help and the leading row has to be the baseline after all —
/// an `Edit`, which left no copy, or a rewrite so thorough that the diff loses
/// to the body it describes.
fn block_against_own_write<'a>(
    mine: &SelfWrite<'_>,
    tag: SectionTag,
    path: &'a Path,
    body: &str,
    text: &'a str,
) -> Option<UpdateBlock<'a>> {
    if mine.still_current {
        return Some(UpdateBlock::SelfEdited { tag, path });
    }
    let base = mine.body?.trim_end_matches('\n');
    // The fingerprint moved but the bytes did not: a `touch`, a re-save, a
    // round trip through an editor. The model's own write is still what the
    // file says, so the pointer remains the true — and shortest — answer.
    if base == body {
        return Some(UpdateBlock::SelfEdited { tag, path });
    }
    Some(UpdateBlock::SinceYourWrite {
        tag,
        path,
        // [`SINCE_YOUR_WRITE_FRAMING`] is deliberately not charged. A plain
        // diff competes with the full body on size because the two carry the
        // same information; this one does not. A body restated in full cannot
        // say that the model's own write was overwritten, or where — the model
        // is left to infer it from a copy that no longer matches what it
        // remembers writing, and the file it is being told about is its own
        // persona. That paragraph is the message, not overhead on it, so the
        // diff only has to beat the body it replaces. It still has to beat it:
        // a wholesale external rewrite quotes both copies and loses, which is
        // the case where inferring is easy anyway.
        diff: worthwhile_diff(base, body, text, 0)?,
    })
}

/// The diff from `prior` to `body`, or `None` when restating `text` in full is
/// the shorter message.
///
/// A rewrite diffs to roughly both copies; then the new body alone is the
/// shorter message, and the one with no reassembly for the model to do.
/// `framing_charge` is the paragraph the first diff of that kind summons,
/// counted against the diff when the diff is competing with the body on size
/// alone — a saving smaller than the paragraph that buys it is not a saving.
/// Charging it to every block of the kind is conservative when there are
/// several, which is the safe direction.
fn worthwhile_diff(prior: &str, body: &str, text: &str, framing_charge: usize) -> Option<String> {
    let diff = unified_diff(prior, body);
    (!diff.is_empty() && diff.len() + framing_charge < text.len()).then_some(diff)
}

/// The body the leading system row carries for `tag`, as
/// [`PromptPart::render`] wrote it — the baseline a diff is taken against.
///
/// `None` when the row has no such section: a `<memory>` block that only
/// appeared after memory was switched on has no prior copy, and there is
/// nothing to diff against. Parses rather than pattern-matches on the whole
/// rendered part because the point is to recover the body when the `path`
/// attribute is exactly what changed.
fn seeded_section_body(seeded: &str, tag: SectionTag) -> Option<&str> {
    let open = format!("<{} path=\"", tag.as_str());
    let attr_start = seeded.find(&open)? + open.len();
    let path_end = attr_start + seeded[attr_start..].find('"')?;
    let body_start = path_end + seeded[path_end..].find(">\n")? + ">\n".len();
    let close = format!("\n</{}>", tag.as_str());
    let body_end = body_start + seeded[body_start..].find(&close)?;
    Some(&seeded[body_start..body_end])
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
    // Each paragraph is paid for only by the update that needs it, so the
    // common single-section delta carries no explanation of cases it lacks.
    let mut framing = FRAMING_BODY.to_string();
    for (present, extra) in [
        (
            blocks.iter().any(|b| matches!(b, UpdateBlock::Diff { .. })),
            DIFF_FRAMING,
        ),
        (
            blocks
                .iter()
                .any(|b| matches!(b, UpdateBlock::SinceYourWrite { .. })),
            SINCE_YOUR_WRITE_FRAMING,
        ),
        (
            blocks
                .iter()
                .any(|b| matches!(b, UpdateBlock::SelfEdited { .. })),
            SELF_EDIT_FRAMING,
        ),
    ] {
        if present {
            framing.push_str("\n\n");
            framing.push_str(extra);
        }
    }
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
        self_written: &HashMap<PathBuf, SelfWrite<'_>>,
    ) -> Vec<UpdateBlock<'a>> {
        build_blocks(parts, rendered, seeded, self_written)
    }

    /// No file in the prompt was written by this conversation.
    fn no_writes() -> HashMap<PathBuf, SelfWrite<'static>> {
        HashMap::new()
    }

    /// The model wrote `path` and the file still holds those bytes.
    fn still_ours(path: &str) -> HashMap<PathBuf, SelfWrite<'static>> {
        HashMap::from([(
            PathBuf::from(path),
            SelfWrite {
                still_current: true,
                body: None,
            },
        )])
    }

    /// The model wrote `path` with `body`, and something has written to the
    /// file since.
    fn overwritten<'a>(path: &str, body: &'a str) -> HashMap<PathBuf, SelfWrite<'a>> {
        HashMap::from([(
            PathBuf::from(path),
            SelfWrite {
                still_current: false,
                body: Some(body),
            },
        )])
    }

    fn render_all(parts: &[PromptPart]) -> Vec<String> {
        parts.iter().map(PromptPart::render).collect()
    }

    /// Whether a rendered update actually carries a diff block. Not a search
    /// for `DIFF_ATTR` alone: that substring also lives inside the word
    /// "different", which the framing uses.
    fn has_diff_block(out: &str) -> bool {
        out.contains(&format!(" {DIFF_ATTR}>"))
    }

    /// A body long enough that a one-line edit to it has an obviously cheaper
    /// description than restating the whole thing.
    fn index(extra: &[&str]) -> String {
        let mut lines: Vec<String> = (0..20)
            .map(|i| format!("- [Memory {i}](m{i}.md) — hook {i}"))
            .collect();
        lines.extend(extra.iter().map(|l| (*l).to_string()));
        lines.join("\n")
    }

    #[test]
    fn a_part_already_in_the_system_row_is_not_reported() {
        let parts = vec![soul("/new", "body")];
        let rendered = render_all(&parts);
        let seeded = format!("preamble\n\n{}\n\ntail", rendered[0]);
        assert!(blocks(&parts, &rendered, &seeded, &no_writes()).is_empty());
    }

    #[test]
    fn a_moved_path_reports_only_the_section_that_moved() {
        let parts = vec![hint("top hint"), soul("/new/SOUL.md", "body"), hint("tail")];
        let rendered = render_all(&parts);
        let seeded = "top hint\n\n<soul path=\"/old/SOUL.md\">\nbody\n</soul>\n\ntail";
        let out = wrap_update(&blocks(&parts, &rendered, seeded, &no_writes()));
        assert!(out.contains("/new/SOUL.md"), "{out}");
        assert!(!out.contains("top hint") && !out.contains("/old/"), "{out}");
    }

    /// The bytes did not change, so re-sending them would be pure duplication:
    /// the new path plus a marker is the whole message.
    #[test]
    fn a_path_only_move_sends_no_body_at_all() {
        let parts = vec![soul("/new/SOUL.md", &index(&[]))];
        let rendered = render_all(&parts);
        let seeded = format!("<soul path=\"/old/SOUL.md\">\n{}\n</soul>", index(&[]));
        let out = wrap_update(&blocks(&parts, &rendered, &seeded, &no_writes()));
        assert!(
            out.contains(&format!("<soul path=\"/new/SOUL.md\" {MOVED_ATTR}/>")),
            "{out}"
        );
        assert!(!out.contains("hook 0"), "the body is unchanged: {out}");
        assert!(!has_diff_block(&out), "nothing differs to diff: {out}");
    }

    /// The case the diff exists for: one memory written mid-conversation.
    #[test]
    fn an_appended_line_sends_that_line_not_the_whole_body() {
        let parts = vec![soul("/w/MEMORY.md", &index(&["- [New](n.md) — fresh"]))];
        let rendered = render_all(&parts);
        let seeded = format!("<soul path=\"/w/MEMORY.md\">\n{}\n</soul>", index(&[]));
        let out = wrap_update(&blocks(&parts, &rendered, &seeded, &no_writes()));

        assert!(has_diff_block(&out), "{out}");
        assert!(out.contains("+- [New](n.md) — fresh"), "{out}");
        assert!(
            !out.contains("hook 0"),
            "lines outside the hunk must not ride along: {out}"
        );
        // The line before the appended one is untouched context, not a
        // `-`/`+` pair — the trap when the last line carries no terminator.
        assert!(
            !out.contains("-- [Memory 19]"),
            "an appended line must not restate its predecessor: {out}"
        );
    }

    /// A diff is a means, not a goal. When the body was rewritten wholesale the
    /// diff quotes both copies, and the new body alone is both shorter and
    /// easier to act on.
    #[test]
    fn a_wholesale_rewrite_falls_back_to_the_full_body() {
        let parts = vec![soul("/w/SOUL.md", "entirely different prose\nsecond line")];
        let rendered = render_all(&parts);
        let seeded = "<soul path=\"/w/SOUL.md\">\nnothing alike here\nanother line\n</soul>";
        let out = wrap_update(&blocks(&parts, &rendered, seeded, &no_writes()));
        assert!(out.contains("entirely different prose"), "{out}");
        assert!(!has_diff_block(&out), "{out}");
        assert!(!out.contains("nothing alike here"), "{out}");
    }

    /// A hint has no tag, so there is no prior copy in the leading row to
    /// address; it goes in full.
    #[test]
    fn a_hint_is_sent_in_full_because_it_has_no_prior_copy_to_diff() {
        let parts = vec![hint("# Memory\n\nrules")];
        let rendered = render_all(&parts);
        let out = wrap_update(&blocks(&parts, &rendered, "stale row", &no_writes()));
        assert!(out.contains("rules"), "{out}");
        assert!(!has_diff_block(&out), "{out}");
    }

    /// A section the row never carried has no baseline either — switching
    /// memory on mid-session must send the index, not a diff against nothing.
    #[test]
    fn a_section_the_leading_row_never_carried_is_sent_in_full() {
        let parts = vec![PromptPart::Section {
            tag: SectionTag::Memory,
            path: PathBuf::from("/w/MEMORY.md"),
            body: index(&[]),
        }];
        let rendered = render_all(&parts);
        let out = wrap_update(&blocks(
            &parts,
            &rendered,
            "<soul path=\"/w/SOUL.md\">\nx\n</soul>",
            &no_writes(),
        ));
        assert!(out.contains("hook 0"), "{out}");
        assert!(!has_diff_block(&out), "{out}");
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
            &no_writes(),
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
        let out = wrap_update(&blocks(&parts, &rendered, "nothing here", &no_writes()));
        assert_eq!(out.matches(OPEN_TAG).count(), 1);
        assert!(out.contains(&format!("{OPEN_TAG}\na\n\nb\n{CLOSE_TAG}")));
    }

    /// A section the model rewrote itself is named, not restated — and the
    /// extra framing paragraph only appears when one is actually present.
    #[test]
    fn a_self_edited_section_is_pointed_at_rather_than_repeated() {
        let parts = vec![soul("/w/SOUL.md", "A VERY LONG BODY THE MODEL JUST WROTE")];
        let rendered = render_all(&parts);
        let mine = still_ours("/w/SOUL.md");
        let out = wrap_update(&blocks(&parts, &rendered, "stale row", &mine));

        assert!(out.contains(SELF_EDITED_ATTR), "{out}");
        assert!(out.contains("<soul path=\"/w/SOUL.md\""), "{out}");
        assert!(
            !out.contains("A VERY LONG BODY"),
            "the body the model wrote must not be echoed back: {out}"
        );
    }

    /// The headline case. The leading row still carries the untouched
    /// template, the model filled it in itself, and somebody has since changed
    /// one line of what it wrote. Diffed against the row that is a wholesale
    /// rewrite and loses to the full body; against the model's own copy it is
    /// the one line that moved.
    #[test]
    fn a_change_after_your_own_write_diffs_against_what_you_wrote_not_the_row() {
        let template = index(&["* **Name:**", "  *(pick something you like)*"]);
        let mine = index(&["* **Name:** the one it picked"]);
        let theirs = index(&["* **Name:** somebody else's idea"]);
        let parts = vec![soul("/w/IDENTITY.md", &theirs)];
        let rendered = render_all(&parts);
        let seeded = format!("<soul path=\"/w/IDENTITY.md\">\n{template}\n</soul>");
        let out = wrap_update(&blocks(
            &parts,
            &rendered,
            &seeded,
            &overwritten("/w/IDENTITY.md", &mine),
        ));

        assert!(out.contains(SINCE_YOUR_WRITE_ATTR), "{out}");
        assert!(out.contains("-* **Name:** the one it picked"), "{out}");
        assert!(out.contains("+* **Name:** somebody else's idea"), "{out}");
        assert!(
            !out.contains("pick something you like"),
            "the row's copy is not the baseline any more: {out}"
        );
        assert!(
            !out.contains("hook 0"),
            "lines outside the hunk must not ride along: {out}"
        );
        // Its own paragraph, and only its own: the block is not diffed against
        // the system prompt, so saying that it is would misdirect.
        assert!(out.contains(SINCE_YOUR_WRITE_FRAMING), "{out}");
        assert!(!out.contains(DIFF_FRAMING), "{out}");
    }

    /// The proportions the reported case really had: a nine-line identity file
    /// with one line changed, where the diff is smaller than the body but not
    /// by a whole framing paragraph. Charging the paragraph to it sends the
    /// file whole — 170 bytes saved, and with them the fact that somebody
    /// overwrote what the model wrote.
    #[test]
    fn a_short_file_still_diffs_against_your_own_write() {
        let filled = |name: &str| {
            format!(
                "# Who Am I?\n\n\
                 *Fill this in during your first conversation. Make it yours.*\n\n\
                 * **Name:** {name}\n\
                 * **Creature:** a small spirit living in the machine, not quite AI, not quite a ghost\n\
                 * **Vibe:** warm and unpretentious, gets things done without any fuss\n\
                 * **Avatar:**\n  \
                 *(workspace-relative path, http(s) URL, or data URI)*"
            )
        };
        let (mine, theirs) = (filled("the one it picked"), filled("somebody else's idea"));
        let parts = vec![soul("/w/IDENTITY.md", &theirs)];
        let rendered = render_all(&parts);
        let out = wrap_update(&blocks(
            &parts,
            &rendered,
            "stale row",
            &overwritten("/w/IDENTITY.md", &mine),
        ));
        assert!(out.contains(SINCE_YOUR_WRITE_ATTR), "{out}");
        assert!(out.contains("-* **Name:** the one it picked"), "{out}");
        assert!(
            !out.contains("# Who Am I?"),
            "outside the hunk, so not restated: {out}"
        );
    }

    /// The fingerprint moved and the bytes did not — a `touch`, a re-save. The
    /// model's own write is still what the file says, so the pointer holds and
    /// the body stays out of the transcript.
    #[test]
    fn a_re_save_that_changed_nothing_is_still_your_edit() {
        let body = index(&["* **Name:** the one it picked"]);
        let parts = vec![soul("/w/IDENTITY.md", &body)];
        let rendered = render_all(&parts);
        let out = wrap_update(&blocks(
            &parts,
            &rendered,
            "stale row",
            &overwritten("/w/IDENTITY.md", &body),
        ));
        assert!(out.contains(SELF_EDITED_ATTR), "{out}");
        assert!(
            !out.contains("hook 0"),
            "nothing changed, so nothing to restate: {out}"
        );
    }

    /// An `Edit` names the fragment it replaced, not the result, so there is no
    /// copy of the model's to diff against and the leading row stays the
    /// baseline — which an edit leaves only slightly stale.
    #[test]
    fn an_edit_leaves_no_copy_so_the_leading_row_stays_the_baseline() {
        let parts = vec![soul("/w/MEMORY.md", &index(&["- [New](n.md) — fresh"]))];
        let rendered = render_all(&parts);
        let seeded = format!("<soul path=\"/w/MEMORY.md\">\n{}\n</soul>", index(&[]));
        let edited = HashMap::from([(
            PathBuf::from("/w/MEMORY.md"),
            SelfWrite {
                still_current: false,
                body: None,
            },
        )]);
        let out = wrap_update(&blocks(&parts, &rendered, &seeded, &edited));
        assert!(has_diff_block(&out), "{out}");
        assert!(out.contains("+- [New](n.md) — fresh"), "{out}");
        assert!(!out.contains(SINCE_YOUR_WRITE_ATTR), "{out}");
    }

    /// The size guard applies to the model's own copy as well: a body replaced
    /// wholesale diffs to both copies whichever baseline it is measured
    /// against, and the new text alone is shorter.
    #[test]
    fn a_wholesale_rewrite_of_your_own_write_falls_back_to_the_full_body() {
        let parts = vec![soul("/w/SOUL.md", "entirely different prose\nsecond line")];
        let rendered = render_all(&parts);
        let out = wrap_update(&blocks(
            &parts,
            &rendered,
            "stale row",
            &overwritten("/w/SOUL.md", "nothing alike here\nanother line"),
        ));
        assert!(out.contains("entirely different prose"), "{out}");
        assert!(!out.contains(SINCE_YOUR_WRITE_ATTR), "{out}");
        assert!(!out.contains("nothing alike here"), "{out}");
    }

    /// Each explanatory paragraph is carried only by an update that needs it.
    #[test]
    fn framing_paragraphs_are_paid_for_only_when_their_case_is_present() {
        let parts = vec![soul("/w/SOUL.md", "body")];
        let rendered = render_all(&parts);
        let out = wrap_update(&blocks(&parts, &rendered, "stale row", &no_writes()));
        assert!(!out.contains(SELF_EDITED_ATTR), "{out}");
        assert_eq!(
            out,
            format!("{FRAMING_BODY}\n\n{OPEN_TAG}\n{}\n{CLOSE_TAG}", rendered[0])
        );
    }

    /// A hint has no file behind it, so it can never be elided even if some
    /// path this conversation wrote happens to be unrelated.
    #[test]
    fn a_hint_is_never_elided_as_self_edited() {
        let parts = vec![hint("# Memory\n\nrules")];
        let rendered = render_all(&parts);
        let mine = still_ours("/w/SOUL.md");
        let out = wrap_update(&blocks(&parts, &rendered, "stale row", &mine));
        assert!(out.contains("rules"), "{out}");
        assert!(!out.contains(SELF_EDITED_ATTR), "{out}");
    }

    /// The parser has to survive the shapes `render` actually emits, including
    /// an empty file and a body whose own text mentions the tag.
    #[test]
    fn the_baseline_parser_reads_back_what_render_wrote() {
        for body in ["", "one line", "a\n\nb", "mentions </soul_ish> inline"] {
            let part = soul("/w/SOUL.md", body);
            let seeded = format!("hint\n\n{}\n\ntail", part.render());
            assert_eq!(
                seeded_section_body(&seeded, SectionTag::Soul),
                Some(body),
                "round-trip failed for {body:?}"
            );
        }
    }
}
