//! In-memory `SecretStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `aura-security` (next to the trait it
//! implements) so crates that depend on `aura-security` but not on
//! `aura-storage` can still spin up a fake store for unit tests.

use std::collections::HashMap;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::secret_store::{Result, SecretStore};

/// In-memory `SecretStore` for tests. Stores raw `(name, encrypted_value)`
/// pairs in a `Mutex<HashMap>`. No encryption performed here — the bytes
/// are whatever the caller hands in (typically already AES-GCM ciphertext
/// from `SecretVault`).
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    data: Mutex<HashMap<String, Vec<u8>>>,
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
}
