use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::ApprovedResource;
use crate::ids::{JobId, SessionId};

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

/// What externally observable signal started this session.
///
/// `Cron` and `System` carry their own contextual reference; `User` is
/// purely "a person typed a message". A session spawned via subagent or
/// user-fork **inherits its trigger from its root session** — the
/// `TriggerSource` answers "who paid for this work" / "what was the
/// business reason", not "who literally constructed this session row".
///
/// Closed strong-typed enum. Extend by adding variants, never by string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSource {
    User,
    Cron { cron_job_id: String },
    System { reason: SystemReason },
}

impl TriggerSource {
    /// Kind discriminator (matches `JobKind` 1:1 — see invariant in
    /// `aura-job`).
    pub fn kind(&self) -> TriggerKind {
        match self {
            TriggerSource::User => TriggerKind::User,
            TriggerSource::Cron { .. } => TriggerKind::Cron,
            TriggerSource::System { .. } => TriggerKind::System,
        }
    }
}

/// Discriminator for `TriggerSource`. Used to enforce the invariant
/// `session.trigger.kind() == job.kind()` at job creation time without
/// having to clone the trigger payload.
///
/// `Spawned` has no `TriggerSource` counterpart (a spawned session
/// inherits its trigger), but spawned **jobs** do — they carry
/// `JobKind::Spawned` while still belonging to a session whose
/// `TriggerSource` reflects the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    User,
    Cron,
    System,
    Spawned,
}

/// Why an internal subsystem started this session.
///
/// Closed enum — add a variant when a new subsystem needs to attribute
/// work back to itself. Never use the open-ended `Other` antipattern.
///
/// Acts as the discriminator-only label persisted on `Session.trigger`.
/// The matching payload-bearing event delivered to the maintenance
/// actor is [`SystemTrigger`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemReason {
    /// `aura review` or equivalent retrospective pass over a prior session.
    HistoryReview,
    /// Background memory consolidation / summarization task.
    MemoryConsolidation,
    /// Async per-session summary refresh — a `BackgroundCompressionRunner` actor
    /// reads its parent's transcript, generates an updated summary, and
    /// writes `<workspace>/state/sessions/<parent_id>/summary.md` plus
    /// the `session_summaries` metadata row. Triggered between turns
    /// from the parent's agent loop. See `docs/background-compression.md`.
    BackgroundCompression,
}

/// Payload carried by a `SystemTrigger::BackgroundCompression`. Built
/// by the parent's agent loop at trigger time and forwarded through
/// `JobInput::System` and `AgentMessage::SystemTrigger` to the
/// maintenance actor's `BackgroundCompressionRunner`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundCompressionPayload {
    pub parent_session_id: SessionId,
    /// Highest `session_messages.ordinal` to include in this pass'
    /// input. Pinned at trigger time so concurrent appends to the
    /// parent don't bleed in mid-pass.
    pub up_to_ordinal: i64,
    /// Opaque owner token stamped onto `session_summaries.in_flight_owner`
    /// when the trigger gate marked the parent in-flight. The runner
    /// uses it for compare-and-clear cleanup so a stale Pass A
    /// finishing after Pass B remarked the parent cannot wipe Pass
    /// B's mark. Generated once per pass at the gate (UUID v4).
    pub in_flight_owner: String,
}

/// Strongly-typed system maintenance trigger. Replaces the previous
/// `(reason: SystemReason, payload: serde_json::Value)` pair carried
/// in `JobInput::System` and `AgentMessage::SystemTrigger`. Unit
/// variants exist for reasons whose runners aren't yet wired
/// (`HistoryReview`, `MemoryConsolidation`); they keep the discriminator
/// space symmetric with [`SystemReason`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SystemTrigger {
    HistoryReview,
    MemoryConsolidation,
    BackgroundCompression(BackgroundCompressionPayload),
}

impl SystemTrigger {
    /// Discriminator label, matching the `Session.trigger` reason field.
    pub fn reason(&self) -> SystemReason {
        match self {
            SystemTrigger::HistoryReview => SystemReason::HistoryReview,
            SystemTrigger::MemoryConsolidation => SystemReason::MemoryConsolidation,
            SystemTrigger::BackgroundCompression(_) => SystemReason::BackgroundCompression,
        }
    }
}

/// Direct parent relationship for sessions spawned from another session.
///
/// `parent_session_id` + `parent_job_id` together pin the **exact moment**
/// in the parent's lifeline that the spawn happened. `kind` distinguishes
/// the two semantically distinct spawn paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    pub parent_session_id: SessionId,
    pub parent_job_id: JobId,
    pub kind: LineageKind,
}

/// How this session was spawned from its parent.
///
/// `Subagent`: the parent agent invoked the spawn-subagent tool inside an
/// LLM iteration; the parent waits synchronously for the child to finish
/// (cancellation propagates down via the cancellation-token tree).
///
/// `UserFork`: a human chose to branch the parent's conversation at a
/// specific job boundary. `fork_at_job_id` and `prefix_state_hash`
/// together let the read-time view UNION the source's prefix with this
/// session's own jobs while detecting source mutation.
///
/// `SystemMaintenance`: an internal, non-user-facing maintenance session
/// (e.g. `SystemReason::BackgroundCompression`) doing work on behalf of its
/// parent. The `Lineage.parent_session_id` pins the session being worked
/// for; cancellation propagates down on parent shutdown via lookup.
/// Maintenance sessions get `is_normal_session = 0` on the row, so default
/// `SessionStore` listings exclude them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LineageKind {
    Subagent,
    UserFork {
        fork_at_job_id: JobId,
        prefix_state_hash: String,
    },
    SystemMaintenance,
}

/// A persisted conversation session — the root container of one trace
/// tree (Job → Step → Span). Trigger and lineage are **orthogonal**:
/// trigger names the business source of work, lineage names the parent
/// session relationship.
///
/// The conversation transcript itself lives in the `aura-context`
/// `ContextManager` for the actor handling this session. `Session`
/// holds only metadata: identifiers, ownership, lineage, soul binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub user: User,
    pub channel: ChannelType,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub state: SessionState,

    /// Topmost ancestor in the lineage chain. Equal to `id` for
    /// root sessions; otherwise points to the ultimate parent. Lets
    /// "all work descended from session X" queries hit one row.
    pub root_session_id: SessionId,

    /// What started this session. A spawned session inherits its
    /// trigger from its root.
    pub trigger: TriggerSource,

    /// Direct parent relationship, present iff this session was spawned
    /// from another (subagent or user-fork).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<Lineage>,

    /// Soul (system prompt / persona) version locked at session creation.
    /// `Job::effective_soul_version` records what was actually loaded at
    /// each job's start; divergence is recorded on `Job.provenance_drift`.
    pub bound_soul_version: String,
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
    fn trigger_source_user_round_trip() {
        let t = TriggerSource::User;
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(s, "{\"kind\":\"user\"}");
        let back: TriggerSource = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.kind(), TriggerKind::User);
    }

    #[test]
    fn trigger_source_cron_round_trip() {
        let t = TriggerSource::Cron {
            cron_job_id: "cron-1".into(),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: TriggerSource = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.kind(), TriggerKind::Cron);
    }

    #[test]
    fn trigger_source_system_round_trip() {
        let t = TriggerSource::System {
            reason: SystemReason::HistoryReview,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: TriggerSource = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.kind(), TriggerKind::System);
    }

    #[test]
    fn lineage_subagent_round_trip() {
        let l = Lineage {
            parent_session_id: SessionId::from("cli-parent"),
            parent_job_id: JobId::new(),
            kind: LineageKind::Subagent,
        };
        let s = serde_json::to_string(&l).unwrap();
        let back: Lineage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn lineage_user_fork_round_trip() {
        let l = Lineage {
            parent_session_id: SessionId::from("cli-source"),
            parent_job_id: JobId::new(),
            kind: LineageKind::UserFork {
                fork_at_job_id: JobId::new(),
                prefix_state_hash: "deadbeef".into(),
            },
        };
        let s = serde_json::to_string(&l).unwrap();
        let back: Lineage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn lineage_system_maintenance_round_trip() {
        let l = Lineage {
            parent_session_id: SessionId::from("user-parent"),
            parent_job_id: JobId::new(),
            kind: LineageKind::SystemMaintenance,
        };
        let s = serde_json::to_string(&l).unwrap();
        let back: Lineage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn trigger_source_system_background_compression_round_trip() {
        let t = TriggerSource::System {
            reason: SystemReason::BackgroundCompression,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: TriggerSource = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.kind(), TriggerKind::System);
    }

    #[test]
    fn system_trigger_unit_variants_round_trip() {
        for t in [
            SystemTrigger::HistoryReview,
            SystemTrigger::MemoryConsolidation,
        ] {
            let s = serde_json::to_string(&t).unwrap();
            let back: SystemTrigger = serde_json::from_str(&s).unwrap();
            assert_eq!(back, t);
            assert_eq!(back.reason(), t.reason());
        }
    }

    #[test]
    fn system_trigger_background_compression_round_trip() {
        let t = SystemTrigger::BackgroundCompression(BackgroundCompressionPayload {
            parent_session_id: SessionId::from("user-1"),
            up_to_ordinal: 42,
            in_flight_owner: "owner-token".into(),
        });
        let s = serde_json::to_string(&t).unwrap();
        let back: SystemTrigger = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.reason(), SystemReason::BackgroundCompression);
    }
}
