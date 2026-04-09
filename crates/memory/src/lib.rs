pub mod error;

pub use error::MemoryError;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, MemoryError>;

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
