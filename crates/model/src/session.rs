use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::ApprovedResource;
use crate::ids::{JobId, SessionId};
use crate::llm_entry_name::LlmEntryName;

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
/// purely "a person typed a message". A session spawned via subagent
/// **inherits its trigger from its root session** — the `TriggerSource`
/// answers "who paid for this work" / "what was the business reason",
/// not "who literally constructed this session row".
///
/// Closed strong-typed enum. Extend by adding variants, never by string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSource {
    User,
    Cron { cron_job_id: String },
}

impl TriggerSource {
    /// Kind discriminator (matches `JobKind` 1:1 — see invariant in
    /// `aura-job`).
    pub fn kind(&self) -> TriggerKind {
        match self {
            TriggerSource::User => TriggerKind::User,
            TriggerSource::Cron { .. } => TriggerKind::Cron,
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

/// Payload carried by a background compression trigger. Built by the
/// parent's agent loop at trigger time and handed to the detached
/// background-summary task the loop spawns in-process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundCompressionPayload {
    /// Highest `session_messages.ordinal` to include in this pass'
    /// input. Pinned at trigger time so concurrent appends to the
    /// parent don't bleed in mid-pass.
    pub up_to_ordinal: i64,
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
    /// The parent's `SpanId` that birthed this session. For
    /// `Subagent`, this is the parent's `ToolCall(spawn_subagent)`
    /// span — disambiguates sibling subagents from the same job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<crate::ids::SpanId>,
    pub kind: LineageKind,
}

/// How this session was spawned from its parent.
///
/// `Subagent`: the parent agent invoked the spawn-subagent tool inside an
/// LLM iteration; the parent waits synchronously for the child to finish
/// (cancellation propagates down via the cancellation-token tree).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LineageKind {
    Subagent,
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
    /// from another (subagent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<Lineage>,

    /// User-facing "hide from my list" flag. Set via the chat admin
    /// `DELETE /v1/chat/sessions/:id` endpoint, which intentionally
    /// does not remove the row — agent state, transcript, and
    /// channel-token all stay live. The chat list endpoint filters
    /// `hidden = true` out; `aura_store::SessionStore::list_all`
    /// does not, so admin / trace browsers continue to see hidden
    /// sessions. Default `false` so legacy JSON blobs deserialize.
    #[serde(default)]
    pub hidden: bool,
}

impl Session {
    /// Whether this session can host background jobs (detached subagents
    /// or detached `Bash` commands). Only a live, registered, top-level
    /// **user** session can run the autonomous notification turn that
    /// delivers a background result, so cron sessions (one-shot +
    /// unregistered) and subagent sessions (their turn ends with the
    /// child) are out of scope and keep blocking / kill-on-timeout
    /// behaviour.
    pub fn supports_background_jobs(&self) -> bool {
        matches!(self.trigger, TriggerSource::User)
            && match &self.lineage {
                None => true,
                Some(l) => !matches!(l.kind, LineageKind::Subagent),
            }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
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

    /// Background-job results (detached subagents and detached `Bash`
    /// commands) that reached a terminal state while the parent actor
    /// was between turns. Drained into a notification turn once no
    /// higher-priority work is queued. Persisted with the session so an
    /// actor evicted by the idle reaper still surfaces the deliveries on
    /// hydration. See `aura_model::spawn_protocol::PendingBackgroundResult`.
    ///
    /// No `serde(alias)` for the old `pending_subagent_results`: that field
    /// held the *old* element shape, which can't deserialize as the new type
    /// — aliasing it would make a whole `Session` row fail to load. Without
    /// the alias serde just ignores the old field (no `deny_unknown_fields`)
    /// and this defaults empty, so an upgrade drops only the transient
    /// in-flight buffer (the results also live in the child trace) rather
    /// than breaking hydration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_background_results: Vec<crate::spawn_protocol::PendingBackgroundResult>,

    /// Barrier cohorts for grouped subagents (`spawn_subagent(group=…)`),
    /// keyed by group name. A member's result is held in its group until
    /// the group is complete (sealed + every member terminal) or its
    /// timeout dissolves it, then released into `pending_background_results`
    /// for one merged notification.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub background_groups: std::collections::HashMap<String, GroupState>,

    /// Which backend created this subagent session, plus (for
    /// External) the agent's `workspace_dir` and `resume_key`.
    /// `None` for non-subagent sessions (top-level user, cron) and
    /// for pre-tag subagent rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_backend: Option<crate::SubagentBackendTag>,

    /// `subagent_type` (profile name) this subagent session was spawned
    /// with, pinned at genesis. Lets a `resume_session_id` call reject a
    /// profile swap — resuming a `planner` child as `general-purpose`
    /// would run a different profile's prompt/contract over the existing
    /// transcript. `None` for non-subagent sessions and pre-pin rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,

    /// Per-session LLM pin: the `aura.json` entry name this session's
    /// turns should resolve against, overriding `default-llm`. `None`
    /// (the default) means "follow the pool default", so a session that
    /// was never switched tracks global `default-llm` changes. Set via
    /// the chat `PUT /v1/chat/sessions/{id}/model` endpoint and read by
    /// the actor spawner (`Router::handle_incoming`) as the loop's
    /// `initial_llm`; a live actor is re-pinned in place via
    /// `AgentMessage::SetModel`. A stranded name (entry later removed)
    /// degrades safely — `LlmClientPool::resolve` falls back to the
    /// default with a warn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_llm: Option<LlmEntryName>,

    /// Reserved extension fields for plugins and experiments.
    #[serde(default)]
    pub extra: HashMap<String, Value>,
}

/// A barrier cohort of grouped subagents. Members' results accumulate in
/// `results` until the group is complete (`sealed` and `results.len() ==
/// expected`) or its timeout elapses, then release into
/// `pending_background_results` as one merged notification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupState {
    /// Member count the dispatching turn issued. The agent loop bumps it
    /// per grouped `spawn_subagent` call; the barrier fires when
    /// `results.len()` reaches it (and the group is sealed).
    pub expected: usize,
    /// Sealed at the end of the dispatching turn — membership is then
    /// final, so the barrier may fire.
    #[serde(default)]
    pub sealed: bool,
    /// Seal time, for the group timeout. `None` until sealed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Held results for members that have reached a terminal state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<crate::spawn_protocol::PendingBackgroundResult>,
}

impl GroupState {
    /// Whether the cohort may fire: sealed, and either every member has a
    /// result (complete) or the group timeout has elapsed since sealing.
    pub fn is_ready(&self, now: chrono::DateTime<chrono::Utc>, timeout: chrono::Duration) -> bool {
        self.sealed
            && (self.results.len() >= self.expected
                || self.sealed_at.is_some_and(|t| now - t >= timeout))
    }

    /// Whether a firing cohort is partial — it timed out before every
    /// member finished, so the stragglers will deliver individually.
    pub fn is_partial(&self) -> bool {
        self.results.len() < self.expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(expected: usize, sealed: bool, results: usize) -> GroupState {
        GroupState {
            expected,
            sealed,
            sealed_at: sealed.then(chrono::Utc::now),
            results: (0..results)
                .map(|i| {
                    crate::spawn_protocol::PendingBackgroundResult::subagent(
                        format!("h{i}"),
                        "explorer",
                        "t",
                        SessionId::from("c"),
                        "r",
                        crate::SubagentExitStatus::Completed,
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn group_not_ready_until_sealed_and_complete() {
        let now = chrono::Utc::now();
        let t = chrono::Duration::minutes(30);
        // Unsealed: never ready even if all results are in.
        assert!(!group(2, false, 2).is_ready(now, t));
        // Sealed but incomplete (1/2): not ready, and would fire partial.
        let partial = group(2, true, 1);
        assert!(!partial.is_ready(now, t));
        assert!(partial.is_partial());
        // Sealed and complete (2/2): ready, not partial.
        let complete = group(2, true, 2);
        assert!(complete.is_ready(now, t));
        assert!(!complete.is_partial());
    }

    #[test]
    fn old_pending_subagent_results_field_is_ignored_not_fatal() {
        // A row persisted by the previous binary carried the OLD element shape
        // under the OLD field name `pending_subagent_results`. The new type
        // can't deserialize those, so the field must be *ignored* (no serde
        // alias) — the row still loads, dropping only the transient buffer.
        let old = r#"{
            "pending_subagent_results": [
                {"handle_id":"bg-1","subagent_type":"explorer","task_summary":"t",
                 "child_session_id":"c","final_text":"r","status":{"kind":"completed"}}
            ]
        }"#;
        let state: SessionState =
            serde_json::from_str(old).expect("an old-shape row must still deserialize");
        assert!(
            state.pending_background_results.is_empty(),
            "the old buffer is dropped, not mis-migrated"
        );
    }

    #[test]
    fn group_times_out_into_partial_fire() {
        // Sealed long ago, still incomplete → ready (timed out) + partial.
        let mut g = group(3, true, 1);
        g.sealed_at = Some(chrono::Utc::now() - chrono::Duration::minutes(31));
        assert!(g.is_ready(chrono::Utc::now(), chrono::Duration::minutes(30)));
        assert!(g.is_partial());
    }

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
    fn lineage_subagent_round_trip() {
        let l = Lineage {
            parent_session_id: SessionId::from("cli-parent"),
            parent_job_id: JobId::new(),
            parent_span_id: Some(crate::ids::SpanId::new()),
            kind: LineageKind::Subagent,
        };
        let s = serde_json::to_string(&l).unwrap();
        let back: Lineage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn lineage_legacy_without_parent_span_id_deserialises_as_none() {
        // Round-trip a Lineage with `parent_span_id: None` (the
        // serialised form skips the field entirely thanks to
        // `skip_serializing_if`), then deserialise back. Equivalent
        // shape to any pre-`parent_span_id` row.
        let l = Lineage {
            parent_session_id: SessionId::from("p"),
            parent_job_id: JobId::new(),
            parent_span_id: None,
            kind: LineageKind::Subagent,
        };
        let s = serde_json::to_string(&l).unwrap();
        assert!(!s.contains("parent_span_id"));
        let back: Lineage = serde_json::from_str(&s).unwrap();
        assert!(back.parent_span_id.is_none());
        assert_eq!(back.kind, LineageKind::Subagent);
    }

    #[test]
    fn background_compression_payload_round_trip() {
        let p = BackgroundCompressionPayload { up_to_ordinal: 42 };
        let s = serde_json::to_string(&p).unwrap();
        let back: BackgroundCompressionPayload = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn session_state_last_llm_round_trip() {
        let state = SessionState {
            last_llm: Some(LlmEntryName::from("claude-opus")),
            ..Default::default()
        };
        let s = serde_json::to_string(&state).unwrap();
        let back: SessionState = serde_json::from_str(&s).unwrap();
        assert_eq!(back.last_llm, Some(LlmEntryName::from("claude-opus")));
    }

    #[test]
    fn session_state_last_llm_defaults_to_none() {
        // Legacy rows persisted before the field existed must deserialize
        // with `last_llm == None`, and an unpinned state must not emit the
        // key (skip_serializing_if) so the JSON stays lean.
        let back: SessionState = serde_json::from_str("{}").unwrap();
        assert_eq!(back.last_llm, None);
        let s = serde_json::to_string(&SessionState::default()).unwrap();
        assert!(
            !s.contains("last_llm"),
            "unset last_llm must not serialize: {s}"
        );
    }
}
