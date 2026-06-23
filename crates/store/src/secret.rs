use async_trait::async_trait;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Async trait for encrypted secret persistence.
///
/// Implemented by `baybo_storage::libsql::LibsqlSecretStore` (production)
/// and `baybo_security::test_support::MemorySecretStore` (tests). The bytes
/// handed in are already AES-256-GCM ciphertext minted by
/// `baybo_security::SecretVault` — this layer only persists opaque blobs
/// keyed by name and never sees plaintext.
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()>;
    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>>;
    async fn list(&self) -> Result<Vec<String>>;
    /// Hard-delete the secret. Later `store` calls with the same name
    /// re-create it. Idempotent on missing names.
    async fn delete(&self, name: &str) -> Result<()>;
}
