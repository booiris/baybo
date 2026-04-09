pub mod error;
pub mod manager;

pub use error::MemoryError;
pub use manager::MemoryManager;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, MemoryError>;

// ---------------------------------------------------------------------------
// MemoryStore trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn store(&self, entry: &MemoryEntry) -> Result<()>;
    async fn retrieve(&self, user_id: &str, key: &str) -> Result<Option<MemoryEntry>>;
    async fn search(&self, user_id: &str, query: &str, limit: usize) -> Result<Vec<MemoryEntry>>;
    async fn delete(&self, id: &str) -> Result<()>;
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<MemoryEntry>>;
}

// ---------------------------------------------------------------------------
// EmbeddingModel trait
// ---------------------------------------------------------------------------

/// A minimal embedding model trait for generating vector embeddings from text.
///
/// Implementations are injected by upper layers (e.g. `agent`). This keeps
/// the `memory` crate free from direct LLM or rig dependencies.
#[async_trait]
pub(crate) trait EmbeddingModel: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

// ---------------------------------------------------------------------------
// MemoryCategory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum MemoryCategory {
    UserPreference,
    KeyFact,
}

// ---------------------------------------------------------------------------
// MemoryEntry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub user_id: String,
    pub content: String,
    pub category: MemoryCategory,
    pub importance: f32,
    pub embedding: Option<Vec<f32>>,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub source_session_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl MemoryEntry {
    /// Create a new `MemoryEntry` with a generated UUID and current timestamps.
    pub fn new(
        user_id: String,
        content: String,
        category: MemoryCategory,
        importance: f32,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            user_id,
            content,
            category,
            importance: importance.clamp(0.0, 1.0),
            embedding: None,
            created_at: now,
            last_accessed: now,
            source_session_id: None,
            expires_at: None,
        }
    }
}
