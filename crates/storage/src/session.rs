use async_trait::async_trait;
use aura_session::{Session, SessionError};
use chrono::{DateTime, Utc};

pub type Result<T> = std::result::Result<T, SessionError>;

/// Abstract interface for session persistence.
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get(&self, session_id: &str) -> Result<Option<Session>>;
    async fn save(&self, session: &Session) -> Result<()>;
    async fn delete(&self, session_id: &str) -> Result<()>;
    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>>;
}
