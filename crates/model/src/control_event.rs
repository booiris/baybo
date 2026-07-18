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
    /// The progress observer's transient narration (`AgentEvent::Progress`) —
    /// the "what's happening now" line shown live as a folded work-block step.
    /// Persisted here so a reload reconstructs it INTO the turn's work block
    /// (unlike a notice, it is not terminal and never gets its own row); the
    /// reconstruction folds it and must never route it through
    /// `control_event_item`, so `notice_level` is `None`.
    Progress,
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
            ControlEventKind::Progress => "progress",
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
            // Not a notice — it folds into the work block, never colors a bar.
            ControlEventKind::Progress => None,
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
            "progress" => Ok(ControlEventKind::Progress),
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
    /// The originating send's `platform_msg_id` for a user-authored `Command`
    /// echo (`/stop`, `/compact`) — empty for the notice kinds. The chat view
    /// surfaces it as the row's `platform_msg_id` so a client's optimistic
    /// command bubble reconciles with this durable echo instead of a difference
    /// sync doubling it. Empty (the historical default) for rows predating the
    /// column and for every non-command event.
    #[serde(default)]
    pub platform_msg_id: String,
}

/// The stable transcript row id of a persisted control event (`n<seq>`), as
/// the sync/backfill DTOs key it. The SINGLE source of the format: the gateway
/// reconstruction and the live `Frame::Notice::durable_id` must produce the
/// same string, because clients dedup a live-minted notice row against its
/// later-synced twin by exactly this id.
pub fn control_event_row_id(seq: i64) -> String {
    format!("n{seq}")
}
