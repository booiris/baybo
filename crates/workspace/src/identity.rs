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
    /// USER.md - long-term user profile.
    pub user: String,
    /// IDENTITY.md - system or instance identity description.
    pub identity: String,
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

/// Load the three identity files from `<root>/profile/`. Any missing
/// file is atomically seeded with its
/// [`IdentityKind::default_content`] before being returned, so the
/// caller is guaranteed a fully-populated [`IdentityFiles`].
///
/// Auto-seeding here means a deleted identity file is recreated on the
/// next session boot. That matches what we want for runtime correctness
/// (the Soul prompt is never half-formed), but it does mean
/// "delete to disable" is not a way to opt out of identity injection —
/// edit the file to be empty instead.
pub async fn load_identity_files(root: &Path) -> anyhow::Result<IdentityFiles> {
    let paths = WorkspacePaths::new(root.to_path_buf());
    let soul_path = paths.identity_file(IdentityKind::Soul);
    let user_path = paths.identity_file(IdentityKind::User);
    let identity_path = paths.identity_file(IdentityKind::Identity);
    let (soul, user, identity) = tokio::try_join!(
        read_or_seed(&soul_path, IdentityKind::Soul.default_content()),
        read_or_seed(&user_path, IdentityKind::User.default_content()),
        read_or_seed(&identity_path, IdentityKind::Identity.default_content()),
    )?;

    Ok(IdentityFiles {
        soul,
        user,
        identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let loaded = load_identity_files(&dir).await.unwrap();
        assert_eq!(loaded.soul, "You are helpful.");

        write_identity_file(&dir, IdentityKind::Soul, "You are concise.")
            .await
            .expect("overwrite soul");
        let loaded = load_identity_files(&dir).await.unwrap();
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

        let files = load_identity_files(&dir).await.unwrap();
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
        let files = load_identity_files(&dir).await.unwrap();
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
