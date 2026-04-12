use std::path::{Path, PathBuf};

/// One of the four well-known workspace identity files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityKind {
    Agents,
    Soul,
    User,
    Identity,
}

impl IdentityKind {
    /// Filename this identity document is stored under, relative to the
    /// workspace root.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Agents => "AGENTS.md",
            Self::Soul => "SOUL.md",
            Self::User => "USER.md",
            Self::Identity => "IDENTITY.md",
        }
    }

    /// Parse a user-facing label (`"agents"`, `"soul"`, `"user"`, `"identity"`).
    /// Accepts any of `name`, uppercase, or `NAME.md`.
    pub fn from_label(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "agents" | "agents.md" => Some(Self::Agents),
            "soul" | "soul.md" => Some(Self::Soul),
            "user" | "user.md" => Some(Self::User),
            "identity" | "identity.md" => Some(Self::Identity),
            _ => None,
        }
    }
}

/// Contents of the workspace identity files.
#[derive(Debug, Clone, Default)]
pub struct IdentityFiles {
    /// AGENTS.md - runtime constraints, roles, and high-level rules.
    pub agents: Option<String>,
    /// SOUL.md - personality, tone, and preferences.
    pub soul: Option<String>,
    /// USER.md - long-term user profile.
    pub user: Option<String>,
    /// IDENTITY.md - system or instance identity description.
    pub identity: Option<String>,
}

/// Reads a file if it exists, returning `None` for missing files.
/// Only propagates genuine I/O errors (permissions, etc.), not "not found".
async fn read_optional_file(path: &Path) -> anyhow::Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write a single identity file atomically (tmpfile + rename).
///
/// Creates `root` if it does not already exist. Returns the absolute path
/// the content was written to. The previous version, if any, is replaced.
pub async fn write_identity_file(
    root: &Path,
    kind: IdentityKind,
    content: &str,
) -> anyhow::Result<PathBuf> {
    tokio::fs::create_dir_all(root).await?;
    let target = root.join(kind.file_name());
    let tmp = target.with_extension("md.tmp");
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, &target).await?;
    Ok(target)
}

/// Loads all identity files from the given workspace root directory.
pub async fn load_identity_files(root: &Path) -> anyhow::Result<IdentityFiles> {
    let agents_path = root.join("AGENTS.md");
    let soul_path = root.join("SOUL.md");
    let user_path = root.join("USER.md");
    let identity_path = root.join("IDENTITY.md");

    let (agents, soul, user, identity) = tokio::try_join!(
        read_optional_file(&agents_path),
        read_optional_file(&soul_path),
        read_optional_file(&user_path),
        read_optional_file(&identity_path),
    )?;

    Ok(IdentityFiles {
        agents,
        soul,
        user,
        identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_label_is_case_insensitive() {
        assert_eq!(IdentityKind::from_label("soul"), Some(IdentityKind::Soul));
        assert_eq!(
            IdentityKind::from_label(" AGENTS "),
            Some(IdentityKind::Agents)
        );
        assert_eq!(
            IdentityKind::from_label("user.md"),
            Some(IdentityKind::User)
        );
        assert_eq!(IdentityKind::from_label("CLAUDE.md"), None);
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
        assert_eq!(path, dir.join("SOUL.md"));

        let loaded = load_identity_files(&dir).await.unwrap();
        assert_eq!(loaded.soul.as_deref(), Some("You are helpful."));

        // Overwrite the same file.
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
        let _ = tokio::fs::create_dir_all(&dir).await;
        tokio::fs::write(dir.join("SOUL.md"), "You are helpful.")
            .await
            .unwrap();

        let files = load_identity_files(&dir).await.unwrap();
        assert_eq!(files.soul.as_deref(), Some("You are helpful."));
        assert!(files.agents.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
