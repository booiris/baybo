//! In-memory `baybo_store::SecretStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `baybo-security` (next to `SecretVault`, the
//! consumer it backs) so a crate building a vault for its own tests gets
//! the fake from the same crate as the vault.

use std::collections::HashMap;

use async_trait::async_trait;
use parking_lot::Mutex;

use baybo_store::secret::{Result, SecretStore, StoreIdentity};

/// In-memory `SecretStore` for tests. Stores raw `(name, encrypted_value)`
/// pairs in a `Mutex<HashMap>`. No encryption performed here — the bytes
/// are whatever the caller hands in (typically already AES-GCM ciphertext
/// from `SecretVault`).
#[derive(Debug)]
pub struct MemorySecretStore {
    data: Mutex<HashMap<String, Vec<u8>>>,
    /// Fresh per instance, so per-credential process state built on top of
    /// a test store is isolated to that test.
    identity: StoreIdentity,
}

impl Default for MemorySecretStore {
    fn default() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
            identity: StoreIdentity::ephemeral(),
        }
    }
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live entries. Useful for asserting deterministic-mint
    /// invariants ("same secret minted twice → vault holds one entry").
    pub fn len(&self) -> usize {
        self.data.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl SecretStore for MemorySecretStore {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()> {
        self.data
            .lock()
            .insert(name.to_owned(), encrypted_value.to_vec());
        Ok(())
    }

    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.data.lock().get(name).cloned())
    }

    async fn list(&self) -> Result<Vec<String>> {
        Ok(self.data.lock().keys().cloned().collect())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        self.data.lock().remove(name);
        Ok(())
    }

    fn identity(&self) -> StoreIdentity {
        self.identity.clone()
    }
}
