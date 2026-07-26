//! Master-key rotation against a real sqlite vault.
//!
//! The unit tests in `baybo-security` run over an in-memory store. This
//! exercises the path that ships: `rewrite_all` inside a sqlite transaction,
//! and the recovery probe reading a real row back.

use std::sync::Arc;

use baybo_security::{EncryptionKey, SecretVault, key_file};
use baybo_storage::Store;
use baybo_store::SecretStore;
use baybo_workspace::WorkspacePaths;

/// Covers the shapes the vault really holds: a fixed-name application record, a
/// minted placeholder, and a non-UTF8 payload (push keys are raw bytes).
const SEED: &[(&str, &[u8])] = &[
    ("gateway.admin_token", b"7f3c1d9e2a48"),
    (
        "[{REDACTED_SECRET_ffa5b8cdc3f14277eacaaebf}]",
        b"sk-live-abc",
    ),
    ("device.d1.push_key", &[0x00, 0xff, 0x10, 0x80, 0x7f]),
];

async fn seed(root: &std::path::Path) -> (WorkspacePaths, Store, EncryptionKey) {
    let paths = WorkspacePaths::new(root.to_path_buf());
    std::fs::create_dir_all(paths.key_dir()).unwrap();
    std::fs::create_dir_all(paths.state_dir()).unwrap();

    let key = EncryptionKey::generate();
    key_file::write(&paths.encryption_key_file(), &key).unwrap();

    let stores = Store::open(paths.storage_db()).await.unwrap();
    let vault = SecretVault::new(key.clone(), stores.secret.clone());
    for (name, value) in SEED {
        vault.store_secret(name, value).await.unwrap();
    }
    (paths, stores, key)
}

#[tokio::test]
async fn rotation_preserves_every_value_and_retires_the_old_key() {
    let dir = tempfile::tempdir().unwrap();
    let (paths, stores, old_key) = seed(dir.path()).await;
    let live = paths.encryption_key_file();

    let lock = baybo_workspace::acquire_workspace_lock(paths.root()).unwrap();
    let vault = SecretVault::new(old_key.clone(), stores.secret.clone());
    let entries = key_file::rotate(&live, &vault, &lock)
        .await
        .expect("rotate");
    assert_eq!(entries, SEED.len());

    let new_key = key_file::load(&live).unwrap();
    assert_ne!(new_key.as_bytes(), old_key.as_bytes(), "key must change");
    assert!(!key_file::pending_path(&live).exists(), "pending consumed");

    let after = SecretVault::new(new_key, stores.secret.clone());
    for (name, value) in SEED {
        let got = after
            .get_secret(name)
            .await
            .unwrap_or_else(|e| panic!("{name} unreadable after rotation: {e}"))
            .unwrap_or_else(|| panic!("{name} vanished"));
        assert_eq!(got.as_bytes(), *value, "{name} changed value");
    }

    let stale = SecretVault::new(old_key, stores.secret.clone());
    assert!(
        stale.get_secret("gateway.admin_token").await.is_err(),
        "the old key must stop opening the vault"
    );
}

/// A rotation interrupted between the sqlite commit and the key-file rename
/// must complete on the next open rather than stranding the workspace.
#[tokio::test]
async fn interrupted_rotation_recovers_on_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let (paths, stores, old_key) = seed(dir.path()).await;
    let live = paths.encryption_key_file();

    // Rotate by hand, stopping just short of promoting the key file.
    let new_key = EncryptionKey::generate();
    key_file::write(&key_file::pending_path(&live), &new_key).unwrap();
    let vault = SecretVault::new(old_key, stores.secret.clone());
    vault.rotate_master_key(&new_key).await.unwrap();

    let store: Arc<dyn SecretStore> = stores.secret.clone();
    let resolved = key_file::resolve_pending(&live, &store)
        .await
        .expect("recovery");

    assert_eq!(resolved.as_bytes(), new_key.as_bytes());
    assert!(!key_file::pending_path(&live).exists());

    let after = SecretVault::new(resolved, stores.secret.clone());
    for (name, value) in SEED {
        assert_eq!(
            after.get_secret(name).await.unwrap().unwrap().as_bytes(),
            *value
        );
    }
}

/// The gateway-stopped requirement is enforced by `rotate` taking the lock as a
/// parameter — it cannot be called without one, so there is no runtime refusal
/// left to test. What remains is that the lock is genuinely exclusive (so the
/// command fails while a gateway holds it) and that rotating does not leak it.
#[tokio::test]
async fn the_workspace_lock_is_exclusive_and_released_after_rotating() {
    let dir = tempfile::tempdir().unwrap();
    let (paths, stores, key) = seed(dir.path()).await;

    let held = baybo_workspace::acquire_workspace_lock(paths.root()).expect("hold");
    assert!(
        baybo_workspace::acquire_workspace_lock(paths.root()).is_err(),
        "a second acquire must fail — this is what stops the CLI while a gateway runs"
    );
    drop(held);

    let lock = baybo_workspace::acquire_workspace_lock(paths.root()).expect("re-acquire");
    let vault = SecretVault::new(key, stores.secret.clone());
    key_file::rotate(&paths.encryption_key_file(), &vault, &lock)
        .await
        .expect("rotate");
    drop(lock);

    assert!(
        baybo_workspace::acquire_workspace_lock(paths.root()).is_ok(),
        "rotation must not leave the workspace locked"
    );
}
