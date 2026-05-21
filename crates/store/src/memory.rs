use async_trait::async_trait;
use aura_model::MemoryEntry;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Persistence interface for memory entries. Implemented by
/// `aura_storage::libsql::LibsqlMemoryStore` (production) and by
/// `aura_memory::test_support::MemoryMemoryStore` (tests).
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn store(&self, entry: &MemoryEntry) -> Result<()>;
    async fn retrieve(&self, user_id: &str, key: &str) -> Result<Option<MemoryEntry>>;
    async fn search(&self, user_id: &str, query: &str, limit: usize) -> Result<Vec<MemoryEntry>>;
    async fn delete(&self, id: &str) -> Result<()>;
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<MemoryEntry>>;

    /// List every memory entry across all users. Operator-facing view used by
    /// `memory list` when no `--user` scope is provided.
    async fn list_all(&self) -> Result<Vec<MemoryEntry>>;

    /// Look an entry up by its stable id alone, without a user scope.
    /// Returns `None` if no entry with that id exists.
    async fn get_by_id(&self, id: &str) -> Result<Option<MemoryEntry>>;
}
