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
    #[serde(default)]
    pub trigger: SessionTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_link: Option<SessionParentLink>,
    pub messages: Vec<ChatMessage>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub state: SessionState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionTrigger {
    #[default]
    User,
    Cron {
        cron_job_id: String,
        scheduled_fire_time: DateTime<Utc>,
    },
    System {
        #[serde(flatten)]
        trigger: SystemTrigger,
    },
    Parent {
        link_kind: ParentLinkKind,
    },
}

// Inner tag is `system_kind` (not `kind`) because `SessionTrigger` is
// internally tagged with `kind` and uses `#[serde(flatten)]` here; sharing
// the tag name would silently drop one of the two discriminators.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "system_kind", rename_all = "snake_case")]
pub enum SystemTrigger {
    PeriodicReview {
        #[serde(default)]
        extra: HashMap<String, Value>,
    },
    ContextCompaction {
        #[serde(default)]
        extra: HashMap<String, Value>,
    },
    MemoryConsolidation {
        #[serde(default)]
        extra: HashMap<String, Value>,
    },
    SkillDiscovery {
        #[serde(default)]
        extra: HashMap<String, Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionParentLink {
    pub session_id: String,
    pub kind: ParentLinkKind,
    pub at_job_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_span_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentLinkKind {
    /// Parent span synchronously waits for this session to terminate
    /// and consumes the result back into its own context.
    SubAgent,
    /// Independent branch off the parent — no result feedback to parent.
    Fork,
    CronChain,
    SystemContinuation,
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

    #[test]
    fn session_trigger_default_is_user() {
        let t: SessionTrigger = Default::default();
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(s, r#"{"kind":"user"}"#);
    }

    #[test]
    fn session_trigger_system_round_trips_without_tag_collision() {
        let t = SessionTrigger::System {
            trigger: SystemTrigger::PeriodicReview {
                extra: HashMap::new(),
            },
        };
        let s = serde_json::to_string(&t).unwrap();
        // Outer `kind` and inner `system_kind` must both be present and distinct.
        assert!(s.contains(r#""kind":"system""#), "outer kind missing: {s}");
        assert!(
            s.contains(r#""system_kind":"periodic_review""#),
            "inner system_kind missing: {s}"
        );

        let back: SessionTrigger = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            SessionTrigger::System {
                trigger: SystemTrigger::PeriodicReview { .. }
            }
        ));
    }

    #[test]
    fn session_trigger_cron_round_trip() {
        let t = SessionTrigger::Cron {
            cron_job_id: "cron-1".into(),
            scheduled_fire_time: Utc::now(),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: SessionTrigger = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, SessionTrigger::Cron { .. }));
    }

    #[test]
    fn legacy_session_without_trigger_deserializes_as_user() {
        let json = r#"{
            "id": "s1",
            "user": {"id":"u1","name":null,"channel":"tui"},
            "channel": "tui",
            "messages": [],
            "created_at": "2026-01-01T00:00:00Z",
            "last_active": "2026-01-01T00:00:00Z",
            "state": {}
        }"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert!(matches!(s.trigger, SessionTrigger::User));
        assert!(s.parent_link.is_none());
    }
}
