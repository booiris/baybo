use async_trait::async_trait;

use crate::SecurityError;

pub type Result<T> = std::result::Result<T, SecurityError>;

/// Async trait for encrypted secret persistence.
///
/// Implemented by `aura_storage::libsql::LibsqlSecretStore` (production)
/// and `aura_security::test_support::MemorySecretStore` (tests). The
/// trait lives here, next to `SecretVault`, so downstream callers and
/// tests can depend on `aura-security` alone for vault-shaped work.
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()>;
    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>>;
    async fn list(&self) -> Result<Vec<String>>;
    /// Hard-delete the secret. Later `store` calls with the same name
    /// re-create it. Idempotent on missing names.
    async fn delete(&self, name: &str) -> Result<()>;
}
