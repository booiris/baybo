//! Step 0 — first-run workspace bootstrap. Idempotent.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aura_config::AuraConfig;
use aura_security::{EncryptionKey, SecretVault};
use aura_storage::Store;
use aura_workspace::WorkspaceManager;
use aura_workspace::WorkspacePaths;
use aura_workspace::paths::ENV_CONFIG_PATH;

use crate::error::{Result, SetupError};

pub struct SetupContext {
    pub config_path: PathBuf,
    pub config: AuraConfig,
    pub vault: Arc<SecretVault>,
    pub stores: Store,
}

pub async fn bootstrap_workspace_if_needed(workspace_root: PathBuf) -> Result<SetupContext> {
    let paths = WorkspacePaths::new(workspace_root.clone());

    WorkspaceManager::new(workspace_root.clone())
        .ensure_layout()
        .await
        .map_err(|e| SetupError::io(workspace_root.clone(), format!("ensure_layout: {e}")))?;

    let key_file = paths.encryption_key_file();
    if !key_file.exists() {
        mint_encryption_key(&key_file)?;
    }

    let config_path = resolve_config_path(&paths);
    let mut config = if config_path.exists() {
        AuraConfig::load_from_file(&config_path)
            .await
            .map_err(|e| SetupError::Config(format!("load {}: {e}", config_path.display())))?
    } else {
        let mut cfg = AuraConfig::default();
        cfg.security.encryption_key_file = Some(key_file.display().to_string());
        cfg
    };

    // Existing config without an explicit key source: pin to ours so the
    // gateway picks it up. Never overwrite an explicit operator choice.
    if config.security.encryption_key_file.is_none()
        && std::env::var(&config.security.encryption_key_env).is_err()
    {
        config.security.encryption_key_file = Some(key_file.display().to_string());
    }

    config
        .validate()
        .map_err(|e| SetupError::Config(format!("validate aura.json: {e}")))?;

    if !config_path.exists() {
        config.write_to_file(&config_path).await.map_err(|e| {
            SetupError::Config(format!("write default {}: {e}", config_path.display()))
        })?;
    }

    let key = load_encryption_key(&key_file)?;

    let stores = Store::open(paths.storage_db())
        .await
        .map_err(|e| SetupError::Storage(format!("open libsql: {e}")))?;

    let vault = Arc::new(SecretVault::new(key, stores.secret.clone()));

    Ok(SetupContext {
        config_path,
        config,
        vault,
        stores,
    })
}

fn resolve_config_path(paths: &WorkspacePaths) -> PathBuf {
    if let Ok(explicit) = std::env::var(ENV_CONFIG_PATH) {
        return PathBuf::from(explicit);
    }
    paths.config_file()
}

fn mint_encryption_key(path: &Path) -> Result<()> {
    let hex = hex::encode(EncryptionKey::generate().as_bytes());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SetupError::io(parent.to_path_buf(), format!("create key dir: {e}")))?;
    }

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| SetupError::io(path.to_path_buf(), format!("create key file: {e}")))?;
    use std::io::Write as _;
    file.write_all(hex.as_bytes())
        .map_err(|e| SetupError::io(path.to_path_buf(), format!("write key bytes: {e}")))?;
    file.write_all(b"\n")
        .map_err(|e| SetupError::io(path.to_path_buf(), format!("write key newline: {e}")))?;
    Ok(())
}

fn load_encryption_key(path: &Path) -> Result<EncryptionKey> {
    let hex_data = std::fs::read_to_string(path)
        .map_err(|e| SetupError::io(path.to_path_buf(), format!("read key file: {e}")))?;
    let bytes = hex::decode(hex_data.trim())
        .map_err(|e| SetupError::Vault(format!("encryption key not valid hex: {e}")))?;
    if bytes.len() != 32 {
        return Err(SetupError::Vault(format!(
            "encryption key must be 32 bytes, got {}",
            bytes.len()
        )));
    }
    EncryptionKey::new(bytes).map_err(|e| SetupError::Vault(format!("invalid encryption key: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[tokio::test]
    async fn first_run_creates_layout_key_and_default_config() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let paths = WorkspacePaths::new(root.clone());

        let ctx = bootstrap_workspace_if_needed(root.clone()).await.unwrap();

        assert!(paths.config_dir().exists());
        assert!(paths.profile_dir().exists());
        assert!(paths.key_dir().exists());
        assert!(paths.encryption_key_file().exists());
        assert!(paths.config_file().exists());
        assert!(paths.storage_db().exists());

        let mode = std::fs::metadata(paths.encryption_key_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "encryption key must be 0600, got {mode:o}");

        assert_eq!(
            ctx.config.security.encryption_key_file.as_deref(),
            Some(paths.encryption_key_file().display().to_string()).as_deref(),
        );
    }

    #[tokio::test]
    async fn second_run_is_idempotent_and_reuses_key() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let paths = WorkspacePaths::new(root.clone());

        bootstrap_workspace_if_needed(root.clone()).await.unwrap();
        let key_first = std::fs::read_to_string(paths.encryption_key_file()).unwrap();
        let cfg_mtime_first = std::fs::metadata(paths.config_file())
            .unwrap()
            .modified()
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bootstrap_workspace_if_needed(root.clone()).await.unwrap();
        let key_second = std::fs::read_to_string(paths.encryption_key_file()).unwrap();
        let cfg_mtime_second = std::fs::metadata(paths.config_file())
            .unwrap()
            .modified()
            .unwrap();

        assert_eq!(key_first, key_second, "key must not be re-minted");
        assert_eq!(
            cfg_mtime_first, cfg_mtime_second,
            "aura.json must not be rewritten on a clean re-run"
        );
    }
}
