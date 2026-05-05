use std::path::{Path, PathBuf};

pub use crate::paths::IdentityKind;
use crate::paths::WorkspacePaths;

/// Contents of the workspace identity files.
#[derive(Debug, Clone, Default)]
pub struct IdentityFiles {
    /// SOUL.md - personality, tone, and preferences.
    pub soul: Option<String>,
    /// USER.md - long-term user profile.
    pub user: Option<String>,
    /// IDENTITY.md - system or instance identity description.
    pub identity: Option<String>,
}

async fn read_optional_file(path: &Path) -> anyhow::Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
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

/// Loads all identity files from the workspace `profile/` directory.
pub async fn load_identity_files(root: &Path) -> anyhow::Result<IdentityFiles> {
    let paths = WorkspacePaths::new(root.to_path_buf());
    let soul_path = paths.identity_file(IdentityKind::Soul);
    let user_path = paths.identity_file(IdentityKind::User);
    let identity_path = paths.identity_file(IdentityKind::Identity);

    let (soul, user, identity) = tokio::try_join!(
        read_optional_file(&soul_path),
        read_optional_file(&user_path),
        read_optional_file(&identity_path),
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
        assert_eq!(loaded.soul.as_deref(), Some("You are helpful."));

        write_identity_file(&dir, IdentityKind::Soul, "You are concise.")
            .await
            .expect("overwrite soul");
        let loaded = load_identity_files(&dir).await.unwrap();
        assert_eq!(loaded.soul.as_deref(), Some("You are concise."));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_load_from_temp_dir() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("workspace_identity_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let profile = dir.join("profile");
        tokio::fs::create_dir_all(&profile).await.unwrap();
        tokio::fs::write(profile.join("SOUL.md"), "You are helpful.")
            .await
            .unwrap();

        let files = load_identity_files(&dir).await.unwrap();
        assert_eq!(files.soul.as_deref(), Some("You are helpful."));
        assert!(files.user.is_none());
        assert!(files.identity.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
