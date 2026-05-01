use async_trait::async_trait;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Async trait for encrypted secret persistence.
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()>;
    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>>;
    async fn list(&self) -> Result<Vec<String>>;
    /// Live names whose key starts with `prefix`. Lets per-bot
    /// callers scope a scan to a `channel.<ct>.bot.<id>.` subtree
    /// without pulling every name in the store into memory just to
    /// filter.
    async fn list_with_prefix(&self, prefix: &str) -> Result<Vec<String>>;
    /// Soft-delete the secret. Later `store` calls with the same name
    /// revive the entry (INSERT OR REPLACE + NULL default on the
    /// `deleted_at` column). Idempotent on missing / already-deleted
    /// names.
    async fn delete(&self, name: &str) -> Result<()>;
    /// Soft-delete every live secret whose name starts with `prefix`.
    /// Used to sweep a per-bot namespace
    /// (`channel.<channel_type>.bot.<bot_id>.`) on bot removal so a
    /// future re-registration under the same `bot_id` doesn't inherit
    /// orphan UATs / OAuth refresh tokens. Returns the number of rows
    /// touched. Idempotent on prefixes that match nothing.
    async fn delete_with_prefix(&self, prefix: &str) -> Result<usize>;
}
