//! Persistence and rotation of the master encryption key.
//!
//! Lives beside [`EncryptionKey`] rather than in the setup wizard: minting a key
//! is a first-run concern, but loading, rotating and recovering one are not, and
//! the boot path needs all three. Splitting them put the crash-recovery step
//! somewhere the gateway never called.
//!
//! Rotation changes two things that cannot be committed together — the key file
//! and every ciphertext in sqlite. The ordering is what makes the gap
//! survivable:
//!
//! 1. write the new key to the pending path
//! 2. re-encrypt every vault entry under it, in one transaction
//! 3. `rename` pending over the live key (atomic)
//!
//! A crash before (2) commits leaves ciphertext under the old key, which the
//! live file still holds. A crash between (2) and (3) leaves ciphertext under
//! the pending key. Either way exactly one file opens the vault, and
//! [`resolve_pending`] finds out which by decrypting a real entry.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use baybo_store::SecretStore;
use baybo_workspace::WorkspaceLock;

use crate::crypto::EncryptionKey;
use crate::secret_vault::SecretVault;
use crate::{Result, SecurityError};

/// Owner-only: the key decrypts every credential in the workspace.
const KEY_FILE_MODE: u32 = 0o600;

/// Suffix of the in-flight key during rotation.
const PENDING_SUFFIX: &str = ".pending";

/// The pending path is *derived* from the live one, never configured
/// separately. The live key's location is operator-configurable
/// (`security.encryption_key_file`), and a pending path resolved from anywhere
/// else — the workspace default, say — would have rotation promote a key the
/// boot path never reads, leaving the vault unopenable.
pub fn pending_path(live: &Path) -> PathBuf {
    let mut name = live.file_name().unwrap_or_default().to_os_string();
    name.push(PENDING_SUFFIX);
    live.with_file_name(name)
}

/// Read a hex-encoded 32-byte key.
pub fn load(path: &Path) -> Result<EncryptionKey> {
    let hex_data = std::fs::read_to_string(path)
        .map_err(|e| SecurityError::Encryption(format!("read key file {}: {e}", path.display())))?;
    let bytes = hex::decode(hex_data.trim())
        .map_err(|e| SecurityError::Encryption(format!("encryption key not valid hex: {e}")))?;
    EncryptionKey::new(bytes)
}

/// Write `key` at [`KEY_FILE_MODE`], replacing any existing file.
///
/// `create_new` is deliberately not used: a pending key left by an aborted
/// attempt is stale by definition and must be overwritable. An existing file
/// also has its mode re-asserted, since `OpenOptions::mode` only applies on
/// creation.
pub fn write(path: &Path, key: &EncryptionKey) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            SecurityError::Encryption(format!("create key dir {}: {e}", parent.display()))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(KEY_FILE_MODE)
        .open(path)
        .map_err(|e| {
            SecurityError::Encryption(format!("create key file {}: {e}", path.display()))
        })?;
    use std::io::Write as _;
    file.write_all(hex::encode(key.as_bytes()).as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|e| SecurityError::Encryption(format!("write {}: {e}", path.display())))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(KEY_FILE_MODE))
        .map_err(|e| SecurityError::Encryption(format!("chmod {}: {e}", path.display())))?;
    Ok(())
}

/// Does `key` decrypt the vault? Answered against a real entry, so it reflects
/// the store rather than any on-disk bookkeeping.
///
/// An empty vault answers `true` for every key — there is nothing to be wrong
/// about, and refusing would strand a workspace interrupted before it stored
/// anything.
async fn opens_vault(key: &EncryptionKey, store: &Arc<dyn SecretStore>) -> Result<bool> {
    let names = store
        .list()
        .await
        .map_err(|e| SecurityError::Storage(e.to_string()))?;
    let Some(probe) = names.first() else {
        return Ok(true);
    };
    let vault = SecretVault::new(key.clone(), Arc::clone(store));
    Ok(vault.get_secret(probe).await.is_ok())
}

/// Load the key that actually opens the vault, finishing or discarding an
/// interrupted rotation on the way.
///
/// **Every path that opens a vault must go through this, not [`load`].** With no
/// pending file it is just `load`; with one, it is the only thing standing
/// between an interrupted rotation and a gateway that starts up unable to
/// decrypt anything.
pub async fn resolve_pending(live: &Path, store: &Arc<dyn SecretStore>) -> Result<EncryptionKey> {
    let live_key = load(live)?;
    let pending = pending_path(live);
    if !pending.exists() {
        return Ok(live_key);
    }

    // The live key still works ⇒ the re-encryption never committed, so the
    // pending key belongs to an attempt that died early.
    if opens_vault(&live_key, store).await? {
        let _ = std::fs::remove_file(&pending);
        tracing::warn!(
            target: "baybo::security",
            "discarded a stale pending encryption key; the live key still opens the vault"
        );
        return Ok(live_key);
    }

    let pending_key = load(&pending)?;
    if !opens_vault(&pending_key, store).await? {
        return Err(SecurityError::Encryption(
            "neither the live nor the pending encryption key opens the vault; restore a key file \
             from backup before starting"
                .into(),
        ));
    }

    std::fs::rename(&pending, live).map_err(|e| {
        SecurityError::Encryption(format!("promote pending key {}: {e}", pending.display()))
    })?;
    tracing::warn!(
        target: "baybo::security",
        "completed an interrupted key rotation; the pending key is now live"
    );
    Ok(pending_key)
}

/// What a rotation produced.
pub struct Rotated {
    pub entries: usize,
    /// Where the pre-rotation key and ciphertext were written.
    pub backup_dir: PathBuf,
}

/// Filenames inside the backup directory. `secrets.sql` restores with
/// `sqlite3 <db> < secrets.sql`, so recovery needs no bespoke command.
const BACKUP_KEY_FILE: &str = "encryption.key";
const BACKUP_SECRETS_FILE: &str = "secrets.sql";

/// Snapshot exactly what rotation is about to overwrite: the outgoing key, and
/// the `secrets` rows as they stand.
///
/// Deliberately **not** a copy of the database. Rotation touches one table;
/// everything else — transcripts, traces, turns — is untouched, so copying it
/// would mean hundreds of megabytes per rotation for no recovery value. These
/// two files are jointly sufficient and individually useless, which is also why
/// they live in one directory.
async fn write_backup(dir: &Path, live: &Path, vault: &SecretVault) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| SecurityError::Encryption(format!("create {}: {e}", dir.display())))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| SecurityError::Encryption(format!("chmod {}: {e}", dir.display())))?;

    std::fs::copy(live, dir.join(BACKUP_KEY_FILE))
        .map_err(|e| SecurityError::Encryption(format!("back up key file: {e}")))?;

    let mut sql = String::from("BEGIN;\n");
    for (name, encrypted) in vault.export_encrypted().await? {
        // Names are hex-cast rather than quoted: minted placeholders are
        // arbitrary text and quoting them by hand is how a restore script
        // silently corrupts one row.
        sql.push_str(&format!(
            "INSERT INTO secrets (name, encrypted_value) VALUES (CAST(X'{}' AS TEXT), X'{}') \
             ON CONFLICT(name) DO UPDATE SET encrypted_value = excluded.encrypted_value;\n",
            hex::encode(name.as_bytes()),
            hex::encode(&encrypted),
        ));
    }
    sql.push_str("COMMIT;\n");

    let path = dir.join(BACKUP_SECRETS_FILE);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(KEY_FILE_MODE)
        .open(&path)
        .map_err(|e| SecurityError::Encryption(format!("create {}: {e}", path.display())))?;
    use std::io::Write as _;
    file.write_all(sql.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| SecurityError::Encryption(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// Re-encrypt the whole vault under a freshly minted key and promote it.
///
/// Writes a backup into `backup_dir` first — the outgoing key plus the current
/// ciphertext — because the operation is otherwise unrecoverable: the old key
/// stops opening the vault at promotion, and a backup taken of only one of the
/// two is worthless. Doing it here rather than asking the caller to means the
/// safety net cannot be forgotten.
///
/// `_lock` is proof the caller holds the workspace singleton, which is what
/// keeps a gateway from starting midway and writing an entry under the outgoing
/// key — outside the snapshot being re-encrypted, and unreadable the moment the
/// new key is promoted. Taking it as a parameter makes that a type-level
/// requirement rather than something a caller has to remember.
pub async fn rotate(
    live: &Path,
    vault: &SecretVault,
    backup_dir: &Path,
    _lock: &WorkspaceLock,
) -> Result<Rotated> {
    write_backup(backup_dir, live, vault).await?;

    let pending = pending_path(live);
    let new_key = EncryptionKey::generate();
    write(&pending, &new_key)?;

    let entries = vault.rotate_master_key(&new_key).await?;

    std::fs::rename(&pending, live).map_err(|e| {
        SecurityError::Encryption(format!("promote new key {}: {e}", pending.display()))
    })?;
    Ok(Rotated {
        entries,
        backup_dir: backup_dir.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemorySecretStore;

    async fn seeded(dir: &Path) -> (PathBuf, Arc<dyn SecretStore>, EncryptionKey) {
        let live = dir.join("encryption.key");
        let key = EncryptionKey::generate();
        write(&live, &key).unwrap();
        let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let vault = SecretVault::new(key.clone(), Arc::clone(&store));
        vault
            .store_secret("gateway.admin_token", b"admin")
            .await
            .unwrap();
        (live, store, key)
    }

    /// The pending path must follow the live key wherever it is configured, or
    /// rotation promotes a key the boot path never reads.
    #[test]
    fn pending_is_derived_from_the_live_path() {
        assert_eq!(
            pending_path(Path::new("/etc/baybo/custom.key")),
            PathBuf::from("/etc/baybo/custom.key.pending")
        );
    }

    #[tokio::test]
    async fn rotation_rewrites_entries_and_promotes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let (live, store, old) = seeded(dir.path()).await;
        let lock = baybo_workspace::acquire_workspace_lock(dir.path()).unwrap();
        let vault = SecretVault::new(old.clone(), Arc::clone(&store));
        let backup = dir.path().join("backup");

        assert_eq!(
            rotate(&live, &vault, &backup, &lock).await.unwrap().entries,
            1
        );
        assert!(!pending_path(&live).exists());

        let new = load(&live).unwrap();
        assert_ne!(new.as_bytes(), old.as_bytes());
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
    }

    /// The backup must be enough to undo a rotation on its own: the outgoing
    /// key plus the ciphertext as it stood. Anything less and the safety net is
    /// decorative.
    #[tokio::test]
    async fn the_backup_restores_the_pre_rotation_state() {
        let dir = tempfile::tempdir().unwrap();
        let (live, store, old) = seeded(dir.path()).await;
        let lock = baybo_workspace::acquire_workspace_lock(dir.path()).unwrap();
        let vault = SecretVault::new(old.clone(), Arc::clone(&store));
        let before = vault.export_encrypted().await.unwrap();
        let backup = dir.path().join("backup");

        rotate(&live, &vault, &backup, &lock).await.unwrap();

        // The archived key is the one that was live, not the new one.
        let archived = load(&backup.join(BACKUP_KEY_FILE)).unwrap();
        assert_eq!(archived.as_bytes(), old.as_bytes());
        assert_ne!(load(&live).unwrap().as_bytes(), old.as_bytes());

        // And the SQL carries every row's pre-rotation ciphertext.
        let sql = std::fs::read_to_string(backup.join(BACKUP_SECRETS_FILE)).unwrap();
        assert!(sql.starts_with("BEGIN;"), "must be one transaction");
        assert!(sql.trim_end().ends_with("COMMIT;"));
        for (name, ciphertext) in &before {
            assert!(
                sql.contains(&hex::encode(ciphertext)),
                "missing ciphertext for an entry"
            );
            assert!(sql.contains(&hex::encode(name.as_bytes())), "missing name");
        }

        // Restoring both puts the vault back: the archived key reads the
        // archived ciphertext.
        for (name, ciphertext) in before {
            store.store(&name, &ciphertext).await.unwrap();
        }
        let restored = SecretVault::new(archived, Arc::clone(&store));
        assert_eq!(
            restored
                .get_secret("gateway.admin_token")
                .await
                .unwrap()
                .unwrap()
                .as_bytes(),
            b"admin"
        );
    }

    /// Both files are credential-equivalent together, so neither may be
    /// group- or world-readable.
    #[tokio::test]
    async fn the_backup_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let (live, store, old) = seeded(dir.path()).await;
        let lock = baybo_workspace::acquire_workspace_lock(dir.path()).unwrap();
        let vault = SecretVault::new(old, Arc::clone(&store));
        let backup = dir.path().join("backup");

        rotate(&live, &vault, &backup, &lock).await.unwrap();

        let mode =
            |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(backup.clone()), 0o700, "backup dir");
        assert_eq!(
            mode(backup.join(BACKUP_SECRETS_FILE)),
            KEY_FILE_MODE,
            "secrets.sql"
        );
        assert_eq!(
            mode(backup.join(BACKUP_KEY_FILE)),
            KEY_FILE_MODE,
            "archived key"
        );
    }

    /// Crash after the re-encryption committed, before the rename.
    #[tokio::test]
    async fn resolve_promotes_a_pending_key_that_opens_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let (live, store, old) = seeded(dir.path()).await;
        let vault = SecretVault::new(old, Arc::clone(&store));

        let new_key = EncryptionKey::generate();
        write(&pending_path(&live), &new_key).unwrap();
        vault.rotate_master_key(&new_key).await.unwrap();

        let resolved = resolve_pending(&live, &store).await.unwrap();
        assert_eq!(resolved.as_bytes(), new_key.as_bytes());
        assert!(!pending_path(&live).exists());
    }

    /// Crash before the re-encryption committed: the pending key is stale.
    #[tokio::test]
    async fn resolve_discards_a_stale_pending_key() {
        let dir = tempfile::tempdir().unwrap();
        let (live, store, key) = seeded(dir.path()).await;
        write(&pending_path(&live), &EncryptionKey::generate()).unwrap();

        let resolved = resolve_pending(&live, &store).await.unwrap();
        assert_eq!(resolved.as_bytes(), key.as_bytes());
        assert!(!pending_path(&live).exists());
    }

    #[tokio::test]
    async fn resolve_refuses_when_no_key_opens_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let (live, store, _key) = seeded(dir.path()).await;
        write(&live, &EncryptionKey::generate()).unwrap();
        write(&pending_path(&live), &EncryptionKey::generate()).unwrap();

        assert!(resolve_pending(&live, &store).await.is_err());
    }

    #[tokio::test]
    async fn key_files_are_owner_only_even_when_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("encryption.key");
        std::fs::write(&live, "stale").unwrap();
        std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o644)).unwrap();

        write(&live, &EncryptionKey::generate()).unwrap();

        let mode = std::fs::metadata(&live).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, KEY_FILE_MODE, "got {mode:o}");
    }
}
