use std::path::{Path, PathBuf};

pub use crate::paths::IdentityKind;
use crate::paths::WorkspacePaths;

/// Contents of the workspace identity files. Every field is always
/// populated: [`load_identity_files`] seeds any missing file from its
/// [`IdentityKind::default_content`] before returning, so callers
/// never have to branch on a `None`.
#[derive(Debug, Clone)]
pub struct IdentityFiles {
    /// SOUL.md - personality, tone, and preferences.
    pub soul: String,
    /// IDENTITY.md - the agent's self-image: name, creature, vibe, emoji,
    /// avatar.
    pub identity: String,
    /// This agent's own USER.md - what it has worked out about the human.
    pub user: String,
    /// The shared `profile/USER.md` every agent reads: the stable facts the
    /// operator curates. `None` when it *is* [`Self::user`] — the built-in's
    /// own notes are the shared profile, so there is nothing to add.
    pub shared_user: Option<String>,
}

/// Read `path`; if it is missing, seed it with `default` and return
/// the same default. The write is staged via a sibling `.tmp` file and
/// renamed so a concurrent reader observes either the previous state
/// or the full default — never a partial.
async fn read_or_seed(path: &Path, default: &str) -> anyhow::Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let tmp = path.with_extension("md.tmp");
            tokio::fs::write(&tmp, default).await?;
            tokio::fs::rename(&tmp, path).await?;
            Ok(default.to_string())
        }
        Err(e) => Err(e.into()),
    }
}

/// Write a single identity file atomically (tmpfile + rename).
///
/// Creates the workspace `profile/` directory if it does not already exist.
/// Returns the absolute path the content was written to. The previous
/// version, if any, is replaced.
pub async fn write_identity_file(
    root: &Path,
    kind: IdentityKind,
    content: &str,
) -> anyhow::Result<PathBuf> {
    let paths = WorkspacePaths::new(root.to_path_buf());
    tokio::fs::create_dir_all(paths.profile_dir()).await?;
    let target = paths.identity_file(kind);
    let tmp = target.with_extension("md.tmp");
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, &target).await?;
    Ok(target)
}

/// Where one identity section is read from, and what to create it with if
/// it is absent.
///
/// A parameter rather than a fixed path because `SOUL.md` and `IDENTITY.md`
/// belong to the *agent*: a session bound to a custom agent reads them from
/// `personas/<id>/`, everything else from `profile/`.
#[derive(Clone, Copy)]
pub struct IdentitySource<'a> {
    pub path: &'a Path,
    pub seed: &'a str,
}

impl<'a> IdentitySource<'a> {
    pub fn new(path: &'a Path, seed: &'a str) -> Self {
        Self { path, seed }
    }
}

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
        out.insert(after_heading, String::new());
        out.insert(after_heading + 1, format!("* **Name:** {name}"));
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

/// The per-agent sections as the *workspace* holds them: the built-in
/// agent's own files under `profile/`, each seeded from its shipped
/// template. Returned as paths because [`IdentitySource`] borrows them.
pub fn workspace_identity_paths(paths: &WorkspacePaths) -> (PathBuf, PathBuf, PathBuf) {
    (
        paths.identity_file(IdentityKind::Soul),
        paths.identity_file(IdentityKind::Identity),
        paths.identity_file(IdentityKind::User),
    )
}

/// Read one identity file, seeding it with `source.seed` if it does not
/// exist yet.
pub async fn load_identity(source: IdentitySource<'_>) -> anyhow::Result<String> {
    read_or_seed(source.path, source.seed).await
}

/// Load the identity sections. All three per-agent files come from the
/// caller-supplied sources; the shared `profile/USER.md` is read here, and
/// returned only when it is a different file from the agent's own — the
/// built-in's notes *are* the shared profile, and emitting it twice would
/// just spend tokens saying the same thing.
///
/// Auto-seeding here means a deleted identity file is recreated on the
/// next session boot. That matches what we want for runtime correctness
/// (the Soul prompt is never half-formed), but it does mean
/// "delete to disable" is not a way to opt out of identity injection —
/// edit the file to be empty instead.
pub async fn load_identity_files(
    root: &Path,
    soul: IdentitySource<'_>,
    identity: IdentitySource<'_>,
    user: IdentitySource<'_>,
) -> anyhow::Result<IdentityFiles> {
    let paths = WorkspacePaths::new(root.to_path_buf());
    let shared_user_path = paths.identity_file(IdentityKind::User);
    let (soul, identity, own_user) = tokio::try_join!(
        read_or_seed(soul.path, soul.seed),
        read_or_seed(identity.path, identity.seed),
        read_or_seed(user.path, user.seed),
    )?;
    let shared_user = if user.path == shared_user_path {
        None
    } else {
        Some(read_or_seed(&shared_user_path, IdentityKind::User.default_content()).await?)
    };

    Ok(IdentityFiles {
        soul,
        identity,
        user: own_user,
        shared_user,
    })
}

/// Materialize one agent's persona directory: create
/// `personas/<agent_id>/skills/`, then write `SOUL.md`, `IDENTITY.md` and
/// `USER.md` **only if absent**, each staged through a sibling `.tmp` file
/// and renamed.
///
/// Idempotent and never destructive — files the agent has since rewritten
/// survive every later call. Run at profile creation and again defensively
/// when an actor for a bound session is built, which is what covers rows
/// created before this existed and files an operator deleted.
pub async fn ensure_persona_layout(
    paths: &WorkspacePaths,
    agent_id: &str,
    seed_soul: &str,
) -> anyhow::Result<()> {
    let skills = paths.persona_skills_dir(agent_id);
    tokio::fs::create_dir_all(&skills)
        .await
        .map_err(|e| anyhow::anyhow!("create persona skills dir {}: {e}", skills.display()))?;
    for (kind, seed) in [
        (IdentityKind::Soul, seed_soul),
        // The self-image template is seeded verbatim: it invites the agent to
        // pick its own name and emoji, and pre-filling it from the profile row
        // would only mint a copy that goes stale on the next rename.
        (
            IdentityKind::Identity,
            IdentityKind::Identity.default_content(),
        ),
    ] {
        let path = paths.persona_identity_file(agent_id, kind);
        read_or_seed(&path, seed)
            .await
            .map_err(|e| anyhow::anyhow!("seed persona {}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Load the workspace's own three sections — i.e. what the built-in
    /// agent reads, both per-agent files coming from `profile/`.
    async fn load_workspace(dir: &Path) -> IdentityFiles {
        let paths = WorkspacePaths::new(dir.to_path_buf());
        let (soul, identity, user) = workspace_identity_paths(&paths);
        load_identity_files(
            dir,
            IdentitySource::new(&soul, IdentityKind::Soul.default_content()),
            IdentitySource::new(&identity, IdentityKind::Identity.default_content()),
            IdentitySource::new(&user, IdentityKind::User.default_content()),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn write_identity_file_creates_dir_and_round_trips() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("identity_write_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        let path = write_identity_file(&dir, IdentityKind::Soul, "You are helpful.")
            .await
            .expect("write soul");
        assert_eq!(path, dir.join("profile").join("SOUL.md"));

        let loaded = load_workspace(&dir).await;
        assert_eq!(loaded.soul, "You are helpful.");

        write_identity_file(&dir, IdentityKind::Soul, "You are concise.")
            .await
            .expect("overwrite soul");
        let loaded = load_workspace(&dir).await;
        assert_eq!(loaded.soul, "You are concise.");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn load_seeds_missing_files_with_defaults() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("workspace_identity_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let profile = dir.join("profile");
        tokio::fs::create_dir_all(&profile).await.unwrap();
        // Only SOUL.md is hand-written; USER.md and IDENTITY.md are absent.
        tokio::fs::write(profile.join("SOUL.md"), "You are helpful.")
            .await
            .unwrap();

        let files = load_workspace(&dir).await;
        assert_eq!(files.soul, "You are helpful.");
        assert_eq!(files.user, IdentityKind::User.default_content());
        assert_eq!(files.identity, IdentityKind::Identity.default_content());

        // After load, the missing files must exist on disk so subsequent
        // direct readers (e.g. the Edit tool) see the same content.
        assert_eq!(
            tokio::fs::read_to_string(profile.join("USER.md"))
                .await
                .unwrap(),
            IdentityKind::User.default_content(),
        );
        assert_eq!(
            tokio::fs::read_to_string(profile.join("IDENTITY.md"))
                .await
                .unwrap(),
            IdentityKind::Identity.default_content(),
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn load_on_fresh_workspace_seeds_all_three() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("identity_fresh_seed_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        // No `profile/` dir at all yet — load must create it and seed
        // every file.
        let files = load_workspace(&dir).await;
        assert_eq!(files.soul, IdentityKind::Soul.default_content());
        assert_eq!(files.user, IdentityKind::User.default_content());
        assert_eq!(files.identity, IdentityKind::Identity.default_content());

        let profile = dir.join("profile");
        for kind in IdentityKind::all() {
            assert!(profile.join(kind.file_name()).exists());
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
