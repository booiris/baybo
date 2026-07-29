use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::ApprovedResource;
use crate::ids::{SessionId, TurnId};
use crate::llm_entry_name::LlmEntryName;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: Option<String>,
    pub channel: ChannelType,
}

/// Open-ended channel identifier, stored as a snake_case string.
///
/// Well-known channels have associated constants (`OWNER`, `TUI`,
/// `TELEGRAM`, `DISCORD`) but the type is deliberately not a closed enum
/// so runtime-registered sidecars can declare arbitrary names (`"slack"`,
/// `"wechat"`, …) without a core enum extension.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChannelType(pub String);

impl ChannelType {
    pub const TUI: &'static str = "tui";
    pub const TELEGRAM: &'static str = "telegram";
    pub const DISCORD: &'static str = "discord";
    pub const WEIXIN: &'static str = "weixin";

    /// The single self-service chat channel the owner's surfaces — web and
    /// mobile — both operate on. One human on however many screens, so there
    /// is one channel, one transcript pool, one identity. The web-vs-device
    /// distinction lives entirely at the auth boundary (the admin bearer vs
    /// an approved device token, and the device store for pairing/push); it
    /// is not a channel. `Subscribed` like `TUI`, but the shared owner
    /// surface rather than the private terminal one.
    pub const OWNER: &'static str = "owner";

    pub fn owner() -> Self {
        Self(Self::OWNER.to_owned())
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
/// `Cron` carries its own contextual reference; `User` is purely "a person
/// typed a message". A session spawned via subagent
/// **inherits its trigger from its root session** — the `TriggerSource`
/// answers "who paid for this work" / "what was the business reason",
/// not "who literally constructed this session row".
///
/// Closed strong-typed enum. Extend by adding variants, never by string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSource {
    User,
    Cron {
        cron_job_id: String,
        /// The conversation that created the cron job, carried onto every
        /// fire session so a `CronCreate` invoked *inside* a fire inherits
        /// the real user conversation as its origin instead of pointing at
        /// the (transient, invisible) fire session.
        #[serde(default)]
        origin_session_id: Option<SessionId>,
        /// Whether this fire session is a first-class conversation: listed
        /// in the chat sidebar, replyable, pushable. True for recurring
        /// fires — each one *is* the notification. False for one-shot fires
        /// (their result is delivered into the origin conversation instead)
        /// and, via the serde default, for every session persisted before
        /// this field existed — so historical fires stay out of the sidebar
        /// with no migration.
        #[serde(default)]
        conversation: bool,
        /// The job's title at the moment this fire was minted. A tombstone,
        /// not a source of truth: clients group a job's fires into one
        /// chat-list row (see `docs/cron-groups.md`) and the gateway labels
        /// that group with the job's *live* title, so a rename propagates
        /// with no rewrite here. This snapshot answers only the one question
        /// the live lookup cannot — "the turn is gone; what was this history
        /// called?" — which is why it is written once at mint and never
        /// updated, and why it cannot go stale. `None` for fires persisted
        /// before it existed; those group only while their turn is alive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_title: Option<String>,
    },
}

impl TriggerSource {
    /// Kind discriminator for this trigger.
    pub fn kind(&self) -> TriggerKind {
        match self {
            TriggerSource::User => TriggerKind::User,
            TriggerSource::Cron { .. } => TriggerKind::Cron,
        }
    }

    /// The cron job this session fired for, or `None` for a user session.
    pub fn cron_job_id(&self) -> Option<&str> {
        match self {
            TriggerSource::User => None,
            TriggerSource::Cron { cron_job_id, .. } => Some(cron_job_id),
        }
    }

    /// The conversation that created this session's cron job, if any. Used
    /// for chained-creation origin inheritance (a `CronCreate` run inside a
    /// fire) and as the delivery target of a one-shot fire's result.
    pub fn cron_origin_session_id(&self) -> Option<&SessionId> {
        match self {
            TriggerSource::User => None,
            TriggerSource::Cron {
                origin_session_id, ..
            } => origin_session_id.as_ref(),
        }
    }

    /// True for a cron fire session that is a first-class conversation
    /// (recurring fires). Chat-list filtering and the push allowlist key off
    /// this: a cron session that is *not* a conversation stays invisible.
    pub fn is_cron_conversation(&self) -> bool {
        matches!(self, TriggerSource::Cron { conversation, .. } if *conversation)
    }

    /// The job's title as it stood when this fire was minted — the fallback
    /// label for a cron group whose job has since been deleted. Prefer the
    /// job's live title when the job still exists.
    pub fn cron_job_title(&self) -> Option<&str> {
        match self {
            TriggerSource::User => None,
            TriggerSource::Cron { job_title, .. } => job_title.as_deref(),
        }
    }
}

/// Discriminator for `TriggerSource`, also recorded on each `Turn` as its
/// `origin` (the owning session's root trigger). `Spawned` has no
/// `TriggerSource` counterpart — a spawned session inherits its parent's
/// trigger — but it is a valid turn origin, since a subagent session's
/// root is itself `Spawned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    User,
    Cron,
    Spawned,
}
/// Direct parent relationship for sessions spawned from another session.
///
/// `parent_session_id` + `parent_turn_id` together pin the **exact moment**
/// in the parent's lifeline that the spawn happened. `kind` distinguishes
/// the two semantically distinct spawn paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    pub parent_session_id: SessionId,
    pub parent_turn_id: TurnId,
    /// The parent's `SpanId` that birthed this session. For
    /// `Subagent`, this is the parent's `ToolCall(spawn_subagent)`
    /// span — disambiguates sibling subagents from the same turn.
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
/// tree (Turn → Step → Span). Trigger and lineage are **orthogonal**:
/// trigger names the business source of work, lineage names the parent
/// session relationship.
///
/// The conversation transcript itself lives in the `baybo-context`
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
    /// `hidden = true` out; `baybo_store::SessionStore::list_all`
    /// does not, so admin / trace browsers continue to see hidden
    /// sessions. Default `false` so legacy JSON blobs deserialize.
    #[serde(default)]
    pub hidden: bool,

    /// User-facing "pin to top" flag for the chat list. Set via the
    /// chat admin `PUT /v1/chat/sessions/:id/pin` endpoint. The chat
    /// list surfaces pinned sessions in their own block above the rest;
    /// it changes presentation only — the row is otherwise an ordinary
    /// session. Like [`Self::hidden`] it is a flat column owned by a
    /// targeted UPDATE (`set_pinned`), not the JSON blob, so a
    /// concurrent `touch` can't clobber it. Default `false` so legacy
    /// JSON blobs deserialize.
    #[serde(default)]
    pub pinned: bool,

    /// User-facing "archived" flag for the chat list. Set via the chat
    /// admin `PUT /v1/chat/sessions/:id/archive` endpoint. Presentation
    /// only — clients group archived rows into their own view; the list
    /// endpoint does NOT filter on it and new activity does not clear
    /// it. Like [`Self::pinned`] it is a flat column owned by a targeted
    /// UPDATE (`set_archived`), not the JSON blob, so a concurrent
    /// `touch` can't clobber it. Default `false` so legacy JSON blobs
    /// deserialize.
    #[serde(default)]
    pub archived: bool,

    /// Which user-created folder this session is filed under in the chat
    /// list (`None` = uncategorized). Set via the chat admin
    /// `PUT /v1/chat/sessions/:id/folder` endpoint. Like [`Self::pinned`]
    /// it is a flat column owned by a targeted UPDATE (`set_folder`), not
    /// the JSON blob, so a concurrent `touch` can't clobber it; `get`
    /// patches it from the column on read. Default `None` so legacy JSON
    /// blobs deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<crate::FolderId>,

    /// Auto-generated conversation title. Stored as a flat column; `get`
    /// patches it from the column on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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
    /// Tool resources the user has granted permanent approval for in this
    /// session. Populated on each `ApproveAlways` decision by the approval
    /// gate; persisted with the session so restored sessions remember the
    /// grants. See `baybo_model::approval` for matching semantics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved_resources: Vec<ApprovedResource>,

    /// Durable state for the complete background-notification pipeline:
    /// collecting terminal turn results, waiting on grouped barriers, and
    /// actively delivering one transcript-backed notification turn.
    ///
    /// Flattening deliberately preserves the existing session JSON keys so
    /// stored rows remain readable in both directions while callers get one
    /// cohesive state object.
    #[serde(default, flatten)]
    pub background_notifications: BackgroundNotificationState,

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

    /// Per-session LLM pin: the `baybo.json` entry name this session's
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

    /// Per-session MODEL pin WITHIN [`Self::last_llm`]'s entry: a specific id
    /// from that entry's `[model] + model_list`. `None` (the default)
    /// means "use the entry's default `model`", so an entry that offers only
    /// one model never needs this set. Sibling to `last_llm` rather than a
    /// change to it: the resolution stays entry-first, and old sessions (no
    /// key) keep resolving to the entry default. Set alongside `last_llm` via
    /// `PUT /v1/chat/sessions/{id}/model`; a stranded model (not among the
    /// entry's candidates) degrades to the entry default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,

    /// Per-session reasoning-effort pin
    /// (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`), or `None` to use the
    /// entry's configured default. Sibling to `last_llm`/`last_model`; the
    /// agent loop carries it onto every `ChatRequest` so the chat header's
    /// thinking level is per-session. Consumed only by openai-subscription.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_effort: Option<String>,

    /// Reserved extension fields for plugins and experiments.
    #[serde(default)]
    pub extra: HashMap<String, Value>,
}

/// The complete durable state of background-result notification delivery.
///
/// `buffered_results` and `active_delivery` are different pipeline stages,
/// not alternatives: while one batch is waiting for a proactive delivery
/// retry, newly finished turns continue accumulating in the buffer for the
/// next batch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackgroundNotificationState {
    /// Terminal background-turn results not yet committed to the transcript as
    /// a notification prompt. The actor merges this buffer into one turn once
    /// higher-priority mailbox work has drained.
    ///
    /// No alias for the older `pending_subagent_results` key: it used an
    /// incompatible element shape. Ignoring that retired key keeps the rest of
    /// a legacy session row hydratable.
    #[serde(
        default,
        rename = "pending_background_results",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub buffered_results: Vec<crate::spawn_protocol::PendingBackgroundResult>,

    /// Barrier cohorts for grouped subagents, keyed by the turn-namespaced
    /// group key. Ready or timed-out groups release their results into
    /// `buffered_results`.
    #[serde(
        default,
        rename = "background_groups",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub groups: HashMap<String, BackgroundNotificationGroup>,

    /// The one batch whose prompt is already durable in the transcript but
    /// whose proactive reply has not yet settled. A fresh batch may remain in
    /// `buffered_results` until this delivery succeeds or degrades to passive.
    #[serde(
        default,
        rename = "pending_notification_turn",
        skip_serializing_if = "Option::is_none"
    )]
    pub active_delivery: Option<BackgroundNotificationDelivery>,
}

impl BackgroundNotificationState {
    /// Whether the actor must stay on its notification timer path.
    pub fn has_work(&self) -> bool {
        !self.buffered_results.is_empty()
            || !self.groups.is_empty()
            || self.active_delivery.is_some()
    }

    /// Whether a terminal result is already owned by any pipeline stage.
    pub fn knows_handle(&self, handle_id: &str) -> bool {
        self.buffered_results
            .iter()
            .any(|result| result.handle_id == handle_id)
            || self.groups.values().any(|group| {
                group
                    .results
                    .iter()
                    .any(|result| result.handle_id == handle_id)
            })
            || self
                .active_delivery
                .as_ref()
                .is_some_and(|delivery| delivery.handle_ids.iter().any(|id| id == handle_id))
    }

    /// Count one successfully dispatched member in a turn-namespaced barrier
    /// cohort. The completion path uses the same key constructor when routing
    /// the member back into this aggregate.
    pub fn register_group_member(&mut self, turn_id: TurnId, group: &str) {
        self.groups
            .entry(BackgroundNotificationGroup::cohort_key(turn_id, group))
            .or_default()
            .expected += 1;
    }
}

/// Retry ledger for the one background-notification batch whose prompt is
/// already in the transcript. It is self-sufficient: `content` freezes the
/// prompt so a compaction-invalidated ordinal can be repaired without reading
/// and reconstructing the original result batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundNotificationDelivery {
    /// Handles owned by this delivery. Besides making the batch auditable,
    /// they keep a re-published terminal event from entering the next buffer
    /// while this batch is awaiting retry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handle_ids: Vec<String>,
    /// The framed notification prompt, exactly as persisted.
    pub content: Vec<crate::ContentBlock>,
    /// `session_messages` ordinal of the persisted prompt row. Dangles after
    /// any compaction (compaction supersedes every active row and re-inserts
    /// at fresh ordinals), which the retry path detects and repairs by
    /// re-appending from `content`.
    pub prompt_ordinal: i64,
    /// Failed delivery attempts so far. At the actor's attempt cap the
    /// ledger clears and delivery degrades to passive.
    #[serde(default, rename = "attempts")]
    pub failed_attempts: u32,
}

/// A barrier cohort of grouped subagents. Members' results accumulate in
/// `results` until the group is complete (`sealed` and `results.len() ==
/// expected`) or its timeout elapses, then release into the notification
/// buffer as one merged notification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackgroundNotificationGroup {
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

impl BackgroundNotificationGroup {
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

    /// The dispatching turn's `turn_id` namespaces the LLM-chosen group name so
    /// reuse in a later turn starts a fresh cohort instead of extending the
    /// earlier one. Both the dispatch and completion paths derive the key here.
    pub fn cohort_key(turn_id: TurnId, group: &str) -> String {
        format!("{turn_id}::{group}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(expected: usize, sealed: bool, results: usize) -> BackgroundNotificationGroup {
        BackgroundNotificationGroup {
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
    fn cohort_key_namespaces_group_by_turn() {
        let j1 = TurnId::new();
        let j2 = TurnId::new();
        // Deterministic per (turn, name): same inputs → same key.
        assert_eq!(
            BackgroundNotificationGroup::cohort_key(j1, "g"),
            BackgroundNotificationGroup::cohort_key(j1, "g")
        );
        // Reusing a name in a different turn (turn) yields a distinct cohort —
        // the property that stops a later turn from extending a prior cohort.
        assert_ne!(
            BackgroundNotificationGroup::cohort_key(j1, "g"),
            BackgroundNotificationGroup::cohort_key(j2, "g")
        );
        // Distinct names within one turn stay distinct.
        assert_ne!(
            BackgroundNotificationGroup::cohort_key(j1, "a"),
            BackgroundNotificationGroup::cohort_key(j1, "b")
        );
    }

    #[test]
    fn group_registration_counts_members_per_turn() {
        let first_turn = TurnId::new();
        let second_turn = TurnId::new();
        let mut notifications = BackgroundNotificationState::default();

        notifications.register_group_member(first_turn, "research");
        notifications.register_group_member(first_turn, "research");
        notifications.register_group_member(second_turn, "research");

        assert_eq!(notifications.groups.len(), 2);
        assert_eq!(
            notifications
                .groups
                .get(&BackgroundNotificationGroup::cohort_key(
                    first_turn, "research"
                ))
                .map(|group| group.expected),
            Some(2)
        );
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
            state.background_notifications.buffered_results.is_empty(),
            "the old buffer is dropped, not mis-migrated"
        );
    }

    /// The old cron delivery cache is dropped, not bridged. On the first boot
    /// of the new binary, an execution that was delivered under the old cache
    /// but whose ledger stamp was lost to a crash can re-deliver once (its
    /// historical transcript row predates the `source_event_id` column, so the
    /// idempotent append can't recognise it). That one-time upgrade duplicate
    /// is accepted — steady state is exactly-once via the unique key. Hydration
    /// itself must never break on the retired key.
    #[test]
    fn legacy_delivered_cron_executions_field_is_ignored() {
        let state: SessionState = serde_json::from_str(
            r#"{
                "delivered_cron_executions": ["execution-1"]
            }"#,
        )
        .expect("legacy cron delivery cache must not break session hydration");

        let current = serde_json::to_value(state).expect("serialize current session state");
        assert!(current.get("delivered_cron_executions").is_none());
    }

    #[test]
    fn background_notification_aggregate_preserves_flat_session_json() {
        let result = crate::spawn_protocol::PendingBackgroundResult::subagent(
            "bg-1",
            "explorer",
            "inspect",
            SessionId::from("child-1"),
            "done",
            crate::SubagentExitStatus::Completed,
        );
        let mut state = SessionState::default();
        state.background_notifications.buffered_results.push(result);
        state.background_notifications.groups.insert(
            "turn::research".into(),
            BackgroundNotificationGroup {
                expected: 2,
                ..BackgroundNotificationGroup::default()
            },
        );
        state.background_notifications.active_delivery = Some(BackgroundNotificationDelivery {
            handle_ids: vec!["bg-active".into()],
            content: vec![crate::ContentBlock::Text("report".into())],
            prompt_ordinal: 7,
            failed_attempts: 2,
        });

        let json = serde_json::to_value(&state).expect("serialize session state");
        assert!(json.get("background_notifications").is_none());
        assert!(json.get("pending_background_results").is_some());
        assert!(json.get("background_groups").is_some());
        assert!(json.get("pending_notification_turn").is_some());

        let mut legacy_json = json.clone();
        legacy_json
            .get_mut("pending_notification_turn")
            .and_then(Value::as_object_mut)
            .expect("serialized delivery object")
            .remove("handle_ids");

        let restored: SessionState =
            serde_json::from_value(legacy_json).expect("deserialize flat legacy-compatible state");
        assert_eq!(
            restored
                .background_notifications
                .buffered_results
                .first()
                .map(|result| result.handle_id.as_str()),
            Some("bg-1")
        );
        assert_eq!(
            restored
                .background_notifications
                .groups
                .get("turn::research")
                .map(|group| group.expected),
            Some(2)
        );
        let delivery = restored
            .background_notifications
            .active_delivery
            .expect("active delivery");
        assert_eq!(delivery.failed_attempts, 2);
        assert!(
            delivery.handle_ids.is_empty(),
            "legacy ledgers predate cross-stage handle dedup"
        );

        let current: SessionState =
            serde_json::from_value(json).expect("deserialize current flat state");
        assert_eq!(
            current
                .background_notifications
                .active_delivery
                .expect("current active delivery")
                .handle_ids,
            ["bg-active"]
        );
    }

    #[test]
    fn active_delivery_participates_in_handle_dedup() {
        let notifications = BackgroundNotificationState {
            active_delivery: Some(BackgroundNotificationDelivery {
                handle_ids: vec!["bg-active".into()],
                content: vec![],
                prompt_ordinal: 1,
                failed_attempts: 0,
            }),
            ..BackgroundNotificationState::default()
        };

        assert!(notifications.knows_handle("bg-active"));
        assert!(!notifications.knows_handle("bg-other"));
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
            origin_session_id: Some(SessionId::from("sess-origin")),
            conversation: true,
            job_title: Some("Morning brief".into()),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: TriggerSource = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.kind(), TriggerKind::Cron);
        assert_eq!(back.cron_job_id(), Some("cron-1"));
        assert_eq!(
            back.cron_origin_session_id().map(|s| s.as_str()),
            Some("sess-origin")
        );
        assert!(back.is_cron_conversation());
        assert_eq!(back.cron_job_title(), Some("Morning brief"));
    }

    /// A cron session persisted before the visibility marker existed must
    /// deserialize as "not a conversation" — that default is what keeps
    /// historical fires out of the chat sidebar with no migration.
    ///
    /// The same default carries the cron-group tombstone: a fire minted before
    /// `job_title` existed has no snapshot, so it can only be grouped while its
    /// turn is alive to supply the label (see `docs/cron-groups.md`).
    #[test]
    fn legacy_cron_trigger_defaults_to_invisible() {
        let back: TriggerSource =
            serde_json::from_str(r#"{"kind":"cron","cron_job_id":"cron-old"}"#).unwrap();
        assert!(!back.is_cron_conversation());
        assert!(back.cron_origin_session_id().is_none());
        assert!(back.cron_job_title().is_none());
    }

    /// The snapshot is a tombstone, not a source of truth — it must survive a
    /// round-trip through the `sessions.data` blob so a group whose turn has
    /// been deleted keeps its name.
    #[test]
    fn cron_job_title_snapshot_survives_a_blob_round_trip() {
        let t = TriggerSource::Cron {
            cron_job_id: "cron-1".into(),
            origin_session_id: None,
            conversation: true,
            job_title: Some("每日晨报".into()),
        };
        let back: TriggerSource =
            serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back.cron_job_title(), Some("每日晨报"));
    }

    #[test]
    fn lineage_subagent_round_trip() {
        let l = Lineage {
            parent_session_id: SessionId::from("cli-parent"),
            parent_turn_id: TurnId::new(),
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
            parent_turn_id: TurnId::new(),
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
    fn session_folder_id_defaults_to_none_and_skips_when_unset() {
        // A legacy session row persisted before folders existed must load
        // with `folder_id == None`, and an uncategorized session must not
        // emit the key (skip_serializing_if) so the blob stays lean.
        let json = r#"{
            "id":"s1",
            "user":{"id":"u1","name":null,"channel":"http"},
            "channel":"http",
            "created_at":"2024-01-01T00:00:00Z",
            "last_active":"2024-01-01T00:00:00Z",
            "state":{},
            "root_session_id":"s1",
            "trigger":{"kind":"user"}
        }"#;
        let sess: Session = serde_json::from_str(json).expect("legacy row without folder_id loads");
        assert_eq!(sess.folder_id, None);
        let s = serde_json::to_string(&sess).unwrap();
        assert!(
            !s.contains("folder_id"),
            "unset folder_id must not serialize: {s}"
        );
        let sess2: Session = serde_json::from_str(json).expect("legacy row without title loads");
        assert_eq!(sess2.title, None);
        assert!(!s.contains("title"), "unset title must not serialize: {s}");
    }

    #[test]
    fn session_title_round_trips() {
        let json = r#"{
            "id":"s1",
            "user":{"id":"u1","name":null,"channel":"http"},
            "channel":"http",
            "created_at":"2024-01-01T00:00:00Z",
            "last_active":"2024-01-01T00:00:00Z",
            "state":{},
            "root_session_id":"s1",
            "trigger":{"kind":"user"},
            "title":"Fix the login redirect bug"
        }"#;
        let sess: Session = serde_json::from_str(json).expect("row with title loads");
        assert_eq!(sess.title.as_deref(), Some("Fix the login redirect bug"));
        let s = serde_json::to_string(&sess).unwrap();
        let back: Session = serde_json::from_str(&s).unwrap();
        assert_eq!(back.title, sess.title);
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
