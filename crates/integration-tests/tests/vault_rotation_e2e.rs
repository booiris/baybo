//! Master-key rotation against a real sqlite vault.
//!
//! The unit tests in `baybo-setup` run over an in-memory store. This exercises
//! the path that actually ships: `rewrite_all` inside a sqlite transaction, and
//! the recovery probe reading a real row back.

use std::sync::Arc;

use baybo_security::{EncryptionKey, SecretVault};
use baybo_setup::rotate::rotate_master_key;
use baybo_storage::Store;
use baybo_store::SecretStore;
use baybo_workspace::WorkspacePaths;

/// Values chosen to cover the shapes the vault really holds: a fixed-name
/// application record, a minted placeholder, and a non-UTF8 payload (push keys
/// are raw bytes).
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
    std::fs::write(
        paths.encryption_key_file(),
        format!("{}\n", hex::encode(key.as_bytes())),
    )
    .unwrap();

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

    let vault = SecretVault::new(old_key.clone(), stores.secret.clone());
    let out = rotate_master_key(&paths, &vault).await.expect("rotate");
    assert_eq!(out.entries, SEED.len());

    let new_key_hex = std::fs::read_to_string(paths.encryption_key_file()).unwrap();
    let new_key = EncryptionKey::new(hex::decode(new_key_hex.trim()).unwrap()).unwrap();
    assert_ne!(new_key.as_bytes(), old_key.as_bytes(), "key must change");

    let after = SecretVault::new(new_key, stores.secret.clone());
    for (name, value) in SEED {
        let got = after
            .get_secret(name)
            .await
            .unwrap_or_else(|e| panic!("{name} unreadable after rotation: {e}"))
            .unwrap_or_else(|| panic!("{name} vanished"));
        assert_eq!(got.as_bytes(), *value, "{name} changed value");
    }

    // The retired key is genuinely retired.
    let stale = SecretVault::new(old_key, stores.secret.clone());
    assert!(
        stale.get_secret("gateway.admin_token").await.is_err(),
        "the old key must stop opening the vault"
    );
}

/// A rotation interrupted between the sqlite commit and the key-file rename
/// must complete on the next boot rather than stranding the workspace.
#[tokio::test]
async fn interrupted_rotation_recovers_on_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let (paths, stores, old_key) = seed(dir.path()).await;

    // Rotate by hand, stopping just short of promoting the key file.
    let new_key = EncryptionKey::generate();
    std::fs::write(
        paths.pending_encryption_key_file(),
        format!("{}\n", hex::encode(new_key.as_bytes())),
    )
    .unwrap();
    let vault = SecretVault::new(old_key, stores.secret.clone());
    vault.rotate_master_key(&new_key).await.unwrap();

    let store: Arc<dyn SecretStore> = stores.secret.clone();
    let resolved = baybo_setup::rotate::resolve_pending_key(&paths, &store)
        .await
        .expect("recovery");

    assert_eq!(resolved.as_bytes(), new_key.as_bytes());
    assert!(!paths.pending_encryption_key_file().exists());

    let after = SecretVault::new(resolved, stores.secret.clone());
    for (name, value) in SEED {
        assert_eq!(
            after.get_secret(name).await.unwrap().unwrap().as_bytes(),
            *value
        );
    }
}
