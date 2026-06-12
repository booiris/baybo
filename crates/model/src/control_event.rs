use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Kind of an out-of-band control/display event (see [`ControlEvent`]). Stored
/// in `session_control_events.kind`; the `notice_*` variants carry the notice
/// severity so a reload colors the bar the way the live frame did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlEventKind {
    /// A user's control-command echo — `text` is what they typed (e.g. `/stop`).
    Command,
    NoticeInfo,
    NoticeWarn,
    NoticeError,
}

impl ControlEventKind {
    /// Canonical lowercase db/wire spelling, matching the
    /// `#[serde(rename_all = "snake_case")]` form.
    pub fn as_str(&self) -> &'static str {
        match self {
            ControlEventKind::Command => "command",
            ControlEventKind::NoticeInfo => "notice_info",
            ControlEventKind::NoticeWarn => "notice_warn",
            ControlEventKind::NoticeError => "notice_error",
        }
    }

    /// Notice severity for a `Notice*` kind (`"info"` / `"warn"` / `"error"`), or
    /// `None` for [`ControlEventKind::Command`]. Single source of truth for the
    /// level vocabulary the chat surface colors by.
    pub fn notice_level(&self) -> Option<&'static str> {
        match self {
            ControlEventKind::Command => None,
            ControlEventKind::NoticeInfo => Some("info"),
            ControlEventKind::NoticeWarn => Some("warn"),
            ControlEventKind::NoticeError => Some("error"),
        }
    }
}

impl std::str::FromStr for ControlEventKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "command" => Ok(ControlEventKind::Command),
            "notice_info" => Ok(ControlEventKind::NoticeInfo),
            "notice_warn" => Ok(ControlEventKind::NoticeWarn),
            "notice_error" => Ok(ControlEventKind::NoticeError),
            other => Err(format!("unknown control event kind: {other}")),
        }
    }
}

/// One out-of-band event in a session's chat transcript that is **not** part of
/// the LLM conversation — a control-command echo (`/stop`, `/compact`) or a
/// notice (`/stop` / `/compact` confirmation, empty-reply fallback). Persisted
/// in `session_control_events` (separate from `session_messages`, which stays
/// exactly the LLM context), interleaved into the chat view by its
/// `after_ordinal` anchor, and never sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlEvent {
    /// Per-session monotonic id — stable client key and same-anchor tiebreak.
    pub seq: i64,
    /// The `session_messages.ordinal` this event follows (`-1` if none existed
    /// yet). The chat view interleaves the event right after that row, so it
    /// lands on the correct page even on scroll-up.
    pub after_ordinal: i64,
    pub kind: ControlEventKind,
    pub text: String,
    /// When the event occurred (e.g. the instant the user hit `/stop`), shown in
    /// the UI.
    pub created_at: DateTime<Utc>,
}
