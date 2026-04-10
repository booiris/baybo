use aura_model::ChatMessage;
use serde::{Deserialize, Serialize};

/// A point-in-time snapshot of the session context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    /// The full message history at the time of the snapshot.
    pub messages: Vec<ChatMessage>,
    /// Total token count at the time of the snapshot.
    pub token_count: usize,
}
