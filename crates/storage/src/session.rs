use async_trait::async_trait;
use aura_model::Session;
use chrono::{DateTime, Utc};

use crate::error::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Abstract interface for session persistence.
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get(&self, session_id: &str) -> Result<Option<Session>>;
    async fn save(&self, session: &Session) -> Result<()>;
    async fn delete(&self, session_id: &str) -> Result<()>;
    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>>;
    /// Return every stored session, ordered by `last_active` descending.
    /// Operator-facing: drives `aura session list` and must not filter by expiry.
    async fn list_all(&self) -> Result<Vec<Session>>;
}
