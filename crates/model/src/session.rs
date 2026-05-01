use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ChatMessage;
use crate::approval::ApprovedResource;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: Option<String>,
    pub channel: ChannelType,
    /// Sidecar tenant id this user reached us through, when known.
    /// Set on inbound messages from multi-bot sidecars (e.g. Lark
    /// where a single sidecar serves multiple bot apps); `None` for
    /// TUI / HTTP / single-tenant channels. Threaded through to
    /// MCP `tools/call` as `_meta.auraBotId` so a sidecar's MCP
    /// server can route to the right tenant in multi-bot
    /// deployments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
}

/// Open-ended channel identifier, stored as a snake_case string.
///
/// Well-known channels have associated constants (`HTTP`, `TUI`,
/// `TELEGRAM`, `DISCORD`) but the type is deliberately not a closed enum
/// so runtime-registered sidecars can declare arbitrary names (`"slack"`,
/// `"wechat"`, …) without a core enum extension.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelType(pub String);

impl ChannelType {
    pub const HTTP: &'static str = "http";
    pub const TUI: &'static str = "tui";
    pub const TELEGRAM: &'static str = "telegram";
    pub const DISCORD: &'static str = "discord";
    pub const WEIXIN: &'static str = "weixin";

    pub fn http() -> Self {
        Self(Self::HTTP.to_owned())
    }

    pub fn tui() -> Self {
        Self(Self::TUI.to_owned())
    }

    pub fn telegram() -> Self {
        Self(Self::TELEGRAM.to_owned())
    }

    pub fn discord() -> Self {
        Self(Self::DISCORD.to_owned())
    }

    pub fn weixin() -> Self {
        Self(Self::WEIXIN.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<&str> for ChannelType {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ChannelType {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for ChannelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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

    /// Tool resources the user has granted permanent approval for in this
    /// session. Populated on each `ApproveAlways` decision by the approval
    /// gate; persisted with the session so restored sessions remember the
    /// grants. See `aura_model::approval` for matching semantics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved_resources: Vec<ApprovedResource>,

    /// Reserved extension fields for plugins and experiments.
    #[serde(default)]
    pub extra: HashMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_type_tui_round_trip() {
        let s = serde_json::to_string(&ChannelType::tui()).unwrap();
        assert_eq!(s, "\"tui\"");
        let back: ChannelType = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ChannelType::tui());
    }

    #[test]
    fn channel_type_open_string_round_trip() {
        let ct = ChannelType::from("slack");
        let s = serde_json::to_string(&ct).unwrap();
        assert_eq!(s, "\"slack\"");
        let back: ChannelType = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ct);
    }
}
