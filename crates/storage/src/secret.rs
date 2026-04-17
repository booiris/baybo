use async_trait::async_trait;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Async trait for encrypted secret persistence.
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()>;
    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>>;
    async fn delete(&self, name: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<String>>;
}
