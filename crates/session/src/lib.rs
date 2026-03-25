mod manager;

pub use manager::SessionManager;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use aura_core::{Result, Session};

/// Abstract interface for session persistence.
///
/// Concrete implementations (e.g. `SqliteSessionStore`) live in the `storage` crate.
/// The trait requires `Send + Sync` so implementations can be shared across threads.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Get a session by session ID.
    /// Returns `None` if the session does not exist (either expired or never created).
    async fn get(&self, session_id: &str) -> Result<Option<Session>>;

    /// Save or update a session.
    /// If the session ID already exists, update it; otherwise insert a new record.
    async fn save(&self, session: &Session) -> Result<()>;

    /// Delete a session by ID.
    /// Used for expiration cleanup or when the user explicitly ends the session.
    async fn delete(&self, session_id: &str) -> Result<()>;

    /// List session IDs whose last activity was before the given time.
    /// Used for batch expiration cleanup.
    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>>;
}
