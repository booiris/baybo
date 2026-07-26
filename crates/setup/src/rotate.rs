//! Master-key rotation, and recovery from an interrupted one.
//!
//! Rotation has to change two things that cannot be committed together: the key
//! file on disk and every ciphertext in sqlite. The ordering here is what makes
//! the gap between them survivable.
//!
//! 1. mint the new key and write it to the **pending** path
//! 2. re-encrypt every vault entry under it, in one transaction
//! 3. promote pending over the live key file (`rename`, atomic)
//!
//! A crash before (2) commits leaves ciphertext under the old key, which the
//! live key file still holds. A crash between (2) and (3) leaves ciphertext
//! under the *pending* key. Either way exactly one of the two files opens the
//! vault, and [`resolve_pending_key`] finds out which by trying to decrypt a
//! real entry rather than guessing.

use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use baybo_security::{EncryptionKey, SecretVault};
use baybo_store::SecretStore;
use baybo_workspace::WorkspacePaths;

use crate::error::{Result, SetupError};

/// Outcome of a completed rotation.
pub struct Rotated {
    pub entries: usize,
}

/// Write `key` to `path` at mode 0600, replacing any existing file.
///
/// `create_new` is deliberately not used: a pending key left by an earlier
/// aborted attempt is stale by definition and must be overwritable.
fn write_key(path: &Path, key: &EncryptionKey) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SetupError::io(parent.to_path_buf(), format!("create key dir: {e}")))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| SetupError::io(path.to_path_buf(), format!("create key file: {e}")))?;
    use std::io::Write as _;
    file.write_all(hex::encode(key.as_bytes()).as_bytes())
        .map_err(|e| SetupError::io(path.to_path_buf(), format!("write key bytes: {e}")))?;
    file.write_all(b"\n")
        .map_err(|e| SetupError::io(path.to_path_buf(), format!("write key newline: {e}")))?;
    file.sync_all()
        .map_err(|e| SetupError::io(path.to_path_buf(), format!("fsync key file: {e}")))?;
    Ok(())
}

pub(crate) fn read_key(path: &Path) -> Result<EncryptionKey> {
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

/// Can `key` decrypt the vault? Answered against a real entry, so it reflects
/// the store rather than any on-disk bookkeeping.
///
/// An empty vault answers `true` for every key — there is nothing to be wrong
/// about, and refusing would strand a workspace whose rotation was interrupted
/// before it stored anything.
async fn key_opens_vault(key: &EncryptionKey, store: &Arc<dyn SecretStore>) -> Result<bool> {
    let names = store
        .list()
        .await
        .map_err(|e| SetupError::Storage(format!("list secrets: {e}")))?;
    let Some(probe) = names.first() else {
        return Ok(true);
    };
    let vault = SecretVault::new(key.clone(), Arc::clone(store));
    Ok(vault.get_secret(probe).await.is_ok())
}

/// Finish or discard a rotation that was interrupted, returning the key that
/// actually opens the vault.
///
/// Called on every boot. With no pending file it just loads the live key.
pub async fn resolve_pending_key(
    paths: &WorkspacePaths,
    store: &Arc<dyn SecretStore>,
) -> Result<EncryptionKey> {
    let live_path = paths.encryption_key_file();
    let pending_path = paths.pending_encryption_key_file();
    let live = read_key(&live_path)?;
    if !pending_path.exists() {
        return Ok(live);
    }

    // Live key still works ⇒ the re-encryption never committed, so the pending
    // key belongs to an attempt that died early and is stale.
    if key_opens_vault(&live, store).await? {
        let _ = std::fs::remove_file(&pending_path);
        tracing::warn!(
            target: "baybo::setup",
            "discarded a stale pending encryption key; the vault still opens with the live key"
        );
        return Ok(live);
    }

    let pending = read_key(&pending_path)?;
    if !key_opens_vault(&pending, store).await? {
        return Err(SetupError::Vault(
            "neither the live nor the pending encryption key opens the vault; restore a key file \
             from backup before starting"
                .into(),
        ));
    }

    std::fs::rename(&pending_path, &live_path)
        .map_err(|e| SetupError::io(live_path.clone(), format!("promote pending key: {e}")))?;
    tracing::warn!(
        target: "baybo::setup",
        "completed an interrupted key rotation: the pending key opens the vault and is now live"
    );
    Ok(pending)
}

/// Re-encrypt the whole vault under a freshly minted master key.
///
/// `vault` must be the only handle in play — a concurrent writer's entry would
/// be written under the old key, outside the snapshot this re-encrypts, and be
/// unreadable afterwards. Callers gate on the workspace singleton lock.
pub async fn rotate_master_key(paths: &WorkspacePaths, vault: &SecretVault) -> Result<Rotated> {
    let live_path = paths.encryption_key_file();
    let pending_path = paths.pending_encryption_key_file();

    let new_key = EncryptionKey::generate();
    write_key(&pending_path, &new_key)?;

    let entries = vault
        .rotate_master_key(&new_key)
        .await
        .map_err(|e| SetupError::Vault(format!("re-encrypt vault: {e}")))?;

    std::fs::rename(&pending_path, &live_path)
        .map_err(|e| SetupError::io(live_path.clone(), format!("promote new key: {e}")))?;

    Ok(Rotated { entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_security::test_support::MemorySecretStore;

    fn paths(root: &Path) -> WorkspacePaths {
        WorkspacePaths::new(root.to_path_buf())
    }

    async fn seeded() -> (tempfile::TempDir, WorkspacePaths, Arc<dyn SecretStore>) {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let key = EncryptionKey::generate();
        write_key(&p.encryption_key_file(), &key).unwrap();
        let vault = SecretVault::new(key, Arc::clone(&store));
        vault
            .store_secret("gateway.admin_token", b"admin")
            .await
            .unwrap();
        vault
            .store_secret("llm.entry.x.api_key", b"llmkey")
            .await
            .unwrap();
        (dir, p, store)
    }

    #[tokio::test]
    async fn rotation_rewrites_every_entry_and_promotes_the_key() {
        let (_d, p, store) = seeded().await;
        let old = read_key(&p.encryption_key_file()).unwrap();
        let vault = SecretVault::new(old.clone(), Arc::clone(&store));

        let out = rotate_master_key(&p, &vault).await.unwrap();
        assert_eq!(out.entries, 2);
        assert!(
            !p.pending_encryption_key_file().exists(),
            "pending must be consumed"
        );

        let new = read_key(&p.encryption_key_file()).unwrap();
        assert_ne!(new.as_bytes(), old.as_bytes());

        // Values survive under the new key, and the old key no longer opens them.
        let after = SecretVault::new(new, Arc::clone(&store));
        assert_eq!(
            after
                .get_secret("gateway.admin_token")
                .await
                .unwrap()
                .unwrap()
                .as_bytes(),
            b"admin"
        );
        let stale = SecretVault::new(old, Arc::clone(&store));
        assert!(stale.get_secret("gateway.admin_token").await.is_err());
    }

    /// Crash after the re-encryption committed but before the rename: the live
    /// file holds the old key, the ciphertext is under the pending one.
    #[tokio::test]
    async fn recovery_promotes_a_pending_key_that_opens_the_vault() {
        let (_d, p, store) = seeded().await;
        let old = read_key(&p.encryption_key_file()).unwrap();
        let vault = SecretVault::new(old.clone(), Arc::clone(&store));

        let new_key = EncryptionKey::generate();
        write_key(&p.pending_encryption_key_file(), &new_key).unwrap();
        vault.rotate_master_key(&new_key).await.unwrap();
        // …and the process dies here, before the rename.

        let resolved = resolve_pending_key(&p, &store).await.unwrap();
        assert_eq!(resolved.as_bytes(), new_key.as_bytes());
        assert!(!p.pending_encryption_key_file().exists());
        assert_eq!(
            read_key(&p.encryption_key_file()).unwrap().as_bytes(),
            new_key.as_bytes()
        );
    }

    /// Crash before the re-encryption committed: the pending key is stale and
    /// must be dropped, not promoted over a working one.
    #[tokio::test]
    async fn recovery_discards_a_stale_pending_key() {
        let (_d, p, store) = seeded().await;
        let live = read_key(&p.encryption_key_file()).unwrap();
        write_key(&p.pending_encryption_key_file(), &EncryptionKey::generate()).unwrap();

        let resolved = resolve_pending_key(&p, &store).await.unwrap();
        assert_eq!(resolved.as_bytes(), live.as_bytes());
        assert!(!p.pending_encryption_key_file().exists());
    }

    /// Neither key working means the vault is genuinely lost; say so instead of
    /// starting with a key that decrypts nothing.
    #[tokio::test]
    async fn recovery_refuses_when_no_key_opens_the_vault() {
        let (_d, p, store) = seeded().await;
        write_key(&p.encryption_key_file(), &EncryptionKey::generate()).unwrap();
        write_key(&p.pending_encryption_key_file(), &EncryptionKey::generate()).unwrap();

        assert!(resolve_pending_key(&p, &store).await.is_err());
    }

    #[tokio::test]
    async fn key_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, p, _store) = seeded().await;
        let mode = std::fs::metadata(p.encryption_key_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }
}
