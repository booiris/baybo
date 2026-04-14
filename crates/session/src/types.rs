use std::collections::HashMap;

use aura_model::ChatMessage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: Option<String>,
    pub channel: ChannelType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelType {
    Telegram,
    Discord,
    Http,
    #[serde(alias = "cli")]
    Tui,
}

impl std::fmt::Display for ChannelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Telegram => write!(f, "telegram"),
            Self::Discord => write!(f, "discord"),
            Self::Http => write!(f, "http"),
            Self::Tui => write!(f, "tui"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub user: User,
    pub channel: ChannelType,
    pub messages: Vec<ChatMessage>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub state: SessionState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    /// Skills currently active on this turn.
    ///
    /// Populated every turn by `AgentLoop` from the explicit-trigger band
    /// (slash command or inline `/mention`, score ≥ 0.9). Multiple may be
    /// active simultaneously; the list is pure provenance for trace and
    /// CLI display — tool governance is computed separately.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_skills: Vec<String>,

    /// Number of context compressions performed in this session.
    /// Incremented after each compression pass; useful for monitoring
    /// or switching compression strategies.
    #[serde(default)]
    pub compression_count: u32,

    /// Reserved extension fields for plugins and experiments.
    #[serde(default)]
    pub extra: HashMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_type_tui_round_trip() {
        let s = serde_json::to_string(&ChannelType::Tui).unwrap();
        assert_eq!(s, "\"tui\"");
        let back: ChannelType = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ChannelType::Tui);
    }

    #[test]
    fn channel_type_deserializes_legacy_cli_alias() {
        let back: ChannelType = serde_json::from_str("\"cli\"").unwrap();
        assert_eq!(back, ChannelType::Tui);
    }
}
