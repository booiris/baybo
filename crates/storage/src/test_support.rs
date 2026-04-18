//! In-memory store implementations for downstream crates' tests.
//!
//! Gated behind the `test-support` cargo feature so they never ship in
//! release builds. Add new fakes here as the trait surface grows; keep
//! each fake colocated with the trait it implements (in this crate's
//! sibling modules) so changing the trait forces an update.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::StorageError;
use crate::secret::{Result as SecretResult, SecretStore};

fn poison<E: std::fmt::Display>(e: E) -> StorageError {
    StorageError::Storage(format!("mutex poisoned: {e}"))
}

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
        self.data.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl SecretStore for MemorySecretStore {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> SecretResult<()> {
        self.data
            .lock()
            .map_err(poison)?
            .insert(name.to_owned(), encrypted_value.to_vec());
        Ok(())
    }

    async fn retrieve(&self, name: &str) -> SecretResult<Option<Vec<u8>>> {
        Ok(self.data.lock().map_err(poison)?.get(name).cloned())
    }

    async fn delete(&self, name: &str) -> SecretResult<()> {
        self.data.lock().map_err(poison)?.remove(name);
        Ok(())
    }

    async fn list(&self) -> SecretResult<Vec<String>> {
        Ok(self.data.lock().map_err(poison)?.keys().cloned().collect())
    }
}
