//! System-prompt (Soul) assembly: the framing that wraps the workspace
//! identity files into the leading `Role::System` row.
//!
//! Moved here from the agent layer so the system-prompt framing lives with
//! the rest of the prompt-injection text. `ContextManager` owns the
//! lifecycle (seed + reseed-after-compaction); this module is the pure
//! assembly seam.

use std::path::{Path, PathBuf};

use baybo_model::AgentProfileId;
use baybo_workspace::{IdentityKind, IdentitySource, WorkspacePaths, absolutise};

/// Minimal fallback used when the workspace soul can't be assembled (an I/O
/// error — identity files normally auto-seed, so this is a last resort). Lives
/// here so the fallback travels with the assembly it backs.
pub const FALLBACK_SYSTEM_PROMPT: &str = "You are Baybo, an intelligent assistant.";

/// Framing preamble prepended to every runtime system prompt. Sets the
/// agent role and points at the per-attribute Edit affordance.
const TOP_HINT: &str = r#"You are an intelligent AI assistant. The following are your core attributes. You should use Edit tool to update the corresponding attribute file according to the conversation content."#;

/// Operating rule for background work, inserted after the identity sections
/// so it reads as runtime behaviour rather than persona. Counters the observed
/// failure where a model, having backgrounded its only remaining task,
/// busy-waits with `sleep` + `JobList` for minutes instead of yielding the
/// turn — the result can only be delivered once the turn ends, so waiting is
/// self-defeating.
const BACKGROUND_TASKS_HINT: &str = r#"# Background work

When a subagent or command runs in the background — you spawned it with `background: true`, or a slow foreground one was auto-converted after its wait — its result is delivered to you automatically as a fresh turn once the CURRENT turn ends. It can never arrive while this turn is still running. So once the only thing left to do is that background job, end the turn: stop calling tools and hand control back. Do NOT `sleep`, and do NOT poll `JobList`, to wait for it — neither can surface the result, they only delay it. `JobList` is a status peek, not a wait or a result channel; an empty `JobList` means nothing is in flight, not that a result is ready. Keep working after backgrounding only if you have other genuinely independent work to do right now."#;

/// Tail appended after every identity section. Lives at the very end so it's
/// the freshest piece of framing right before the conversation begins — the
/// model reads tag-handling guidance immediately before it encounters the
/// first message that may carry one.
const TAIL_HINT: &str = r#"Tool results and user messages may include <system-reminder> or other tags. Tags contain information from the system. They bear no direct relation to the specific tool results or user messages in which they appear."#;

/// Separator between the parts of an assembled prompt. Load-bearing beyond
/// formatting: [`crate::prompts::system_prompt_update`] tests a stored prompt
/// for a part by substring, so a part has to appear in the joined text exactly
/// as [`AssembledPrompt::parts`] hands it over.
const PART_SEPARATOR: &str = "\n\n";

/// The `<tag path="…">` sections a prompt can carry. Owns the five tag names,
/// which the assembly writes and the freshness reconciler reasons about — one
/// source rather than a literal per call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionTag {
    Soul,
    Identity,
    SharedUserProfile,
    UserNotes,
    Memory,
}

impl SectionTag {
    pub fn as_str(self) -> &'static str {
        match self {
            SectionTag::Soul => "soul",
            SectionTag::Identity => "identity",
            SectionTag::SharedUserProfile => "shared_user_profile",
            SectionTag::UserNotes => "user_notes",
            SectionTag::Memory => "memory",
        }
    }
}

/// One element of an assembled prompt: a block of framing text, or an identity
/// file wrapped with the path it was read from.
///
/// The split exists because the reconciler treats the two differently. A hint
/// is opaque text that only a binary upgrade changes. A section names a file the
/// model can be shown to have edited itself, which is the one case where
/// restating its body is telling the model something it already knows.
pub enum PromptPart {
    Hint(String),
    Section {
        tag: SectionTag,
        /// Absolute, as the rendered `path="…"` attribute carries it — so a
        /// comparison against a tool call's `file_path` is a comparison of the
        /// same string the model was handed.
        path: PathBuf,
        body: String,
    },
}

impl PromptPart {
    /// The part as it appears in the prompt.
    pub fn render(&self) -> String {
        match self {
            PromptPart::Hint(text) => text.clone(),
            PromptPart::Section { tag, path, body } => format!(
                "<{tag} path=\"{path}\">\n{body}\n</{tag}>",
                tag = tag.as_str(),
                path = path.display(),
                body = body.trim_end_matches('\n'),
            ),
        }
    }

    /// The file this part was read from, for a section; `None` for a hint.
    pub fn source_path(&self) -> Option<&Path> {
        match self {
            PromptPart::Hint(_) => None,
            PromptPart::Section { path, .. } => Some(path),
        }
    }
}

/// A system prompt as its ordered parts — the hint blocks and the wrapped
/// identity sections — rather than one opaque string.
///
/// The parts are what the freshness reconciler works in: it compares a live
/// assembly against the prompt a session's leading `Role::System` row already
/// carries, and reports the parts that differ. Joining is the last step, so
/// nothing has to re-split a prompt to find its seams.
pub struct AssembledPrompt {
    parts: Vec<PromptPart>,
}

impl AssembledPrompt {
    /// A prompt with no seams: a subagent's profile, which comes from the
    /// in-process registry rather than from anything the workspace assembles.
    /// It changes as one block or not at all, so it is one part.
    pub fn opaque(text: String) -> Self {
        Self {
            parts: vec![PromptPart::Hint(text)],
        }
    }

    /// The prompt as the leading `Role::System` row carries it.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .map(PromptPart::render)
            .collect::<Vec<_>>()
            .join(PART_SEPARATOR)
    }

    /// The parts, in the order they appear in [`Self::text`].
    pub fn parts(&self) -> &[PromptPart] {
        &self.parts
    }

    /// What this prompt costs, per identity file, on every request the session
    /// makes. The dream pass is handed these numbers because the instruction it
    /// runs on ("keep them lean") is otherwise an adjective with nothing behind
    /// it — a pass with no sense of the budget trims until the diff feels big
    /// enough and stops, which is what it did.
    pub fn budget(&self, tokenizer: &dyn crate::tokenizer::Tokenizer) -> PromptBudget {
        let mut budget = PromptBudget::default();
        for part in &self.parts {
            let cost = tokenizer.count_text(&part.render());
            budget.total += cost;
            if let PromptPart::Section { tag, .. } = part {
                let slot = match tag {
                    SectionTag::Soul => &mut budget.soul,
                    SectionTag::Identity => &mut budget.identity,
                    SectionTag::SharedUserProfile => &mut budget.shared_user_profile,
                    SectionTag::UserNotes => &mut budget.user_notes,
                    SectionTag::Memory => &mut budget.memory_index,
                };
                *slot += cost;
            }
        }
        budget
    }
}

/// Per-section token cost of an assembled prompt.
///
/// Every field but [`Self::memory_index`] is a file that rides the prompt in
/// full and is therefore paid on every call; the memory index is the one that
/// buys deferred reads, so it is reported beside them as the cheaper place to
/// put detail.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct PromptBudget {
    pub soul: usize,
    pub identity: usize,
    pub user_notes: usize,
    pub shared_user_profile: usize,
    pub memory_index: usize,
    /// The whole prompt, hints included.
    pub total: usize,
}

/// Which identity files an agent's prompt is assembled from, and what to seed
/// each with when it does not exist yet.
///
/// Keyed on the agent alone. The profile row is not consulted — not even for
/// its existence: the files are named by the id the session carries, so
/// deleting a profile leaves every bound conversation with the persona it has
/// been talking to.
pub struct PersonaSources {
    pub soul_path: PathBuf,
    pub soul_seed: String,
    pub self_image_path: PathBuf,
    pub self_image_seed: String,
    pub user_notes_path: PathBuf,
    pub user_notes_seed: String,
    /// This agent's `MEMORY.md`, or `None` when file memory is disabled.
    /// Resolved from the same binding as the identity files, so a session can
    /// never read one agent's soul with another's memory.
    pub memory_index: Option<PathBuf>,
}

impl PersonaSources {
    pub fn for_agent(paths: &WorkspacePaths, agent: &AgentProfileId, builtin_memory: bool) -> Self {
        let path = |kind: IdentityKind| agent.identity_file(paths, kind);
        // The same table setup and profile creation seed from. A file the
        // operator deleted must come back as what shipped, not as whatever
        // this path happened to name — recreating the built-in's `SOUL.md`
        // from the custom-agent skeleton would rewrite who the assistant is,
        // permanently and silently.
        let seed = |kind| baybo_workspace::identity::persona_seed(agent.as_str(), kind).to_string();
        Self {
            soul_path: path(IdentityKind::Soul),
            soul_seed: seed(IdentityKind::Soul),
            self_image_path: path(IdentityKind::Identity),
            self_image_seed: seed(IdentityKind::Identity),
            user_notes_path: path(IdentityKind::User),
            user_notes_seed: seed(IdentityKind::User),
            memory_index: builtin_memory.then(|| agent.memory_index_file(paths)),
        }
    }
}

/// [`assemble`] against a resolved [`PersonaSources`], for callers that hold an
/// agent id rather than four paths.
pub async fn assemble_for(
    paths: &WorkspacePaths,
    sources: &PersonaSources,
) -> anyhow::Result<AssembledPrompt> {
    assemble(
        paths,
        IdentitySource::new(&sources.soul_path, &sources.soul_seed),
        IdentitySource::new(&sources.self_image_path, &sources.self_image_seed),
        IdentitySource::new(&sources.user_notes_path, &sources.user_notes_seed),
        sources.memory_index.as_deref(),
    )
    .await
}

/// Size past which a memory index is worth complaining about. Not a cap —
/// see [`memory_parts`] for why truncating would be worse — just the point
/// where "one line per memory" has stopped being a rounding error against a
/// context window (~8 KiB is on the order of 2k tokens, spent on every
/// single request the session makes).
const MEMORY_INDEX_WARN_BYTES: usize = 8 * 1024;

/// How to use the memory tree whose index the `<memory>` section carries.
/// Follows that section so the rules arrive with the index they govern, and
/// is omitted wholesale when file memory is disabled.
///
/// `{{memory_dir}}` is substituted with the absolute directory this session's
/// memory lives in — per-agent, so the path differs between agents.
const MEMORY_HINT: &str = r#"# Memory

Your memory lives in `{{memory_dir}}`: one markdown file per remembered fact, indexed by the `<memory>` block above. The index is always in front of you; the files are not. When an index line looks relevant to what you are doing, `Read` that file — never answer from the one-line hook alone.

Write a memory as soon as you learn something worth carrying into a later conversation: a durable fact about your human, a correction they gave you, the state of ongoing work, a pointer to something external. Do not record what the sections above already say, what matters only inside this conversation, or anything you could re-derive by looking it up.

One fact per file. Create it at `{{memory_dir}}/<slug>.md`:

---
name: <short-kebab-case-slug>
description: <one line — this is what the index shows>
metadata:
  type: user | feedback | project | reference
---

<the fact; link related memories with [[their-name]]>

`user` — who your human is. `feedback` — how they have asked you to work, and why. `project` — ongoing work and where it stands; write dates absolutely, never "last week". `reference` — pointers to external things (URLs, dashboards, tickets).

Then add its line to `{{memory_dir}}/MEMORY.md` in the same breath — `- [Title](file.md) — hook` — because a memory the index does not name will never be found again. Before creating a file, check whether one already covers the fact and update that one instead. When a memory turns out to be wrong or obsolete, delete it with `MemoryDelete` and drop its index line. Keep the index skimmable: it rides every prompt you will ever run."#;

/// Assemble the system prompt: [`TOP_HINT`] up front (agent role + Edit
/// affordance), the identity sections, the `<memory>` index plus
/// [`MEMORY_HINT`], then [`BACKGROUND_TASKS_HINT`] and [`TAIL_HINT`]
/// (operating rules last, so the declarative content reads as one block).
///
/// `memory_index` is `None` when built-in memory is disabled, which drops the
/// section and its rules together — a prompt that taught the model to write
/// memories it has nowhere to put would be worse than silence. Reads the
/// files via the auto-seeding `load_identity_files`, so a deleted file is
/// recreated rather than left half-formed.
///
/// All three sources belong to the agent, which reads
/// `personas/<id>/{SOUL,IDENTITY,USER}.md` — the built-in included, at
/// `personas/baybo/`. Each carries what to create that file with if it does
/// not exist yet.
///
/// `user_notes` is the agent's *own* record of the human. The stable facts the
/// operator curates live in the shared `personas/USER.md`, which belongs to no
/// agent and is always emitted as its own `<shared_user_profile>` section.
pub async fn assemble(
    paths: &WorkspacePaths,
    soul: IdentitySource<'_>,
    self_image: IdentitySource<'_>,
    user_notes: IdentitySource<'_>,
    memory_index: Option<&Path>,
) -> anyhow::Result<AssembledPrompt> {
    let identity =
        baybo_workspace::identity::load_identity_files(paths.root(), soul, self_image, user_notes)
            .await?;
    let mut parts = vec![
        PromptPart::Hint(TOP_HINT.to_string()),
        section(SectionTag::Soul, soul.path, &identity.soul),
        section(SectionTag::Identity, self_image.path, &identity.identity),
        section(
            SectionTag::SharedUserProfile,
            &paths.shared_user_file(),
            &identity.shared_user,
        ),
        section(SectionTag::UserNotes, user_notes.path, &identity.user),
    ];
    if let Some(index_path) = memory_index {
        parts.extend(memory_parts(index_path).await);
    }
    parts.push(PromptPart::Hint(BACKGROUND_TASKS_HINT.to_string()));
    parts.push(PromptPart::Hint(TAIL_HINT.to_string()));
    Ok(AssembledPrompt { parts })
}

/// The `<memory>` index section plus the rules for using it, or nothing at
/// all if the index can't be read.
///
/// Memory is auxiliary to the persona: a workspace whose memory dir is
/// unreadable should still get a coherent system prompt, so this failure
/// warns and degrades to "no memory this session" instead of failing the
/// assembly and dropping the session to [`FALLBACK_SYSTEM_PROMPT`].
async fn memory_parts(index_path: &Path) -> Vec<PromptPart> {
    let index = match baybo_workspace::load_memory_index(index_path).await {
        Ok(index) => index,
        Err(e) => {
            tracing::warn!(error = %e, path = %index_path.display(), "memory index unreadable; assembling without memory");
            return Vec::new();
        }
    };
    // The index rides message[0] of every call this session makes, so its
    // size is a per-turn tax forever — and it grows by a line per memory
    // with nothing but the prompt's own "keep it skimmable" to stop it.
    // Warn rather than truncate: a cut index would point the model at
    // memories it can no longer see, which is worse than an expensive one.
    // The operator can prune, and the dream pass is asked to.
    if index.len() > MEMORY_INDEX_WARN_BYTES {
        tracing::warn!(
            path = %index_path.display(),
            bytes = index.len(),
            "memory index is large and rides every request in this session; \
             consider pruning it"
        );
    }
    // The dir, not the index file: every instruction in the hint addresses
    // a sibling of the index.
    let dir = index_path
        .parent()
        .map(absolutise)
        .unwrap_or_else(|| absolutise(index_path));
    vec![
        section(SectionTag::Memory, index_path, &index),
        PromptPart::Hint(MEMORY_HINT.replace("{{memory_dir}}", &dir.display().to_string())),
    ]
}

/// An identity-file body wrapped in an XML tag carrying the absolute on-disk
/// path. Explicit boundaries keep arbitrary user-authored markdown inside one
/// file from bleeding into a sibling section, and surfacing the path lets the
/// agent re-read or update the source file without re-deriving its location.
fn section(tag: SectionTag, path: &Path, body: &str) -> PromptPart {
    PromptPart::Section {
        tag,
        path: absolutise(path),
        body: body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_workspace::IdentityKind;

    #[tokio::test]
    async fn assembles_hint_sections_and_tail_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(dir.path().to_path_buf());
        let file =
            |kind| paths.persona_identity_file(baybo_workspace::paths::BUILTIN_PERSONA_DIR, kind);
        let (soul_path, identity_path, user_path) = (
            file(IdentityKind::Soul),
            file(IdentityKind::Identity),
            file(IdentityKind::User),
        );
        let prompt = assemble(
            &paths,
            IdentitySource::new(&soul_path, IdentityKind::Soul.default_content()),
            IdentitySource::new(&identity_path, IdentityKind::Identity.default_content()),
            IdentitySource::new(&user_path, IdentityKind::User.default_content()),
            None,
        )
        .await
        .expect("assemble")
        .text();

        assert!(prompt.starts_with("You are an intelligent AI assistant."));
        let soul = prompt.find("<soul ").expect("soul tag");
        let identity = prompt.find("<identity ").expect("identity tag");
        let shared = prompt
            .find("<shared_user_profile ")
            .expect("shared_user_profile tag");
        let notes = prompt.find("<user_notes ").expect("user_notes tag");
        let background = prompt.find("# Background work").expect("background hint");
        let tail = prompt
            .find("Tool results and user messages may include <system-reminder>")
            .expect("tail hint");
        assert!(soul < identity && identity < shared && shared < notes);
        assert!(notes < background && background < tail);
        assert!(prompt.trim_end().ends_with(
            "They bear no direct relation to the specific tool results or user messages in which they appear."
        ));
    }

    #[tokio::test]
    async fn the_memory_section_carries_the_index_and_names_its_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(dir.path().to_path_buf());
        let index = paths.persona_memory_index_file("baybo");
        tokio::fs::create_dir_all(paths.persona_memory_dir("baybo"))
            .await
            .expect("mkdir");
        tokio::fs::write(&index, "# Memory Index\n\n- [Cat](cat.md) — Mochi\n")
            .await
            .expect("write index");

        let prompt = assemble(
            &paths,
            IdentitySource::new(
                &paths.persona_identity_file("baybo", IdentityKind::Soul),
                "s",
            ),
            IdentitySource::new(
                &paths.persona_identity_file("baybo", IdentityKind::Identity),
                "i",
            ),
            IdentitySource::new(
                &paths.persona_identity_file("baybo", IdentityKind::User),
                "u",
            ),
            Some(&index),
        )
        .await
        .expect("assemble")
        .text();

        assert!(prompt.contains("- [Cat](cat.md) — Mochi"));
        // Operating rules come after the declarative content.
        let memory = prompt.find("<memory ").expect("memory tag");
        let hint = prompt.find("# Memory\n").expect("memory hint");
        let background = prompt.find("# Background work").expect("background hint");
        assert!(memory < hint && hint < background);
        // A leftover placeholder would send every write to a literal path.
        assert!(!prompt.contains("{{memory_dir}}"));
        let memory_dir = absolutise(&paths.persona_memory_dir("baybo"));
        assert!(prompt.contains(&format!("Your memory lives in `{}`", memory_dir.display())));
    }

    #[tokio::test]
    async fn disabled_builtin_memory_drops_the_section_and_its_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(dir.path().to_path_buf());

        let prompt = assemble(
            &paths,
            IdentitySource::new(
                &paths.persona_identity_file("baybo", IdentityKind::Soul),
                "s",
            ),
            IdentitySource::new(
                &paths.persona_identity_file("baybo", IdentityKind::Identity),
                "i",
            ),
            IdentitySource::new(
                &paths.persona_identity_file("baybo", IdentityKind::User),
                "u",
            ),
            None,
        )
        .await
        .expect("assemble")
        .text();

        assert!(!prompt.contains("<memory "));
        assert!(!prompt.contains("# Memory\n"));
        // …and nothing is seeded on disk for a feature that is switched off.
        assert!(!paths.persona_memory_dir("baybo").exists());
        assert!(prompt.contains("<soul "));
    }
}
