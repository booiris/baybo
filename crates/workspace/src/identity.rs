use std::path::Path;

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
